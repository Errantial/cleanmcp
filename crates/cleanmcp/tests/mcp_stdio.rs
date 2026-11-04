//! End-to-end: run `cleanmcp mcp` as a child process and talk to it with the rmcp client.

use std::fs;
use std::path::Path;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams, ResourceContents};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};

type Client = RunningService<rmcp::RoleClient, ()>;

async fn start(args: &[&str], data_dir: &Path) -> Client {
    let exe = env!("CARGO_BIN_EXE_cleanmcp");
    let transport = TokioChildProcess::new(tokio::process::Command::new(exe).configure(|cmd| {
        cmd.arg("mcp").args(args);
        // Keep the user's real config and audit log out of tests.
        cmd.env("CLEANMCP_CONFIG", data_dir.join("none.toml"));
        cmd.env("CLEANMCP_DATA_DIR", data_dir);
        cmd.env("CLEANMCP_TEST_TRASH_DIR", data_dir);
    }))
    .expect("spawn cleanmcp");
    ().serve(transport).await.expect("initialize")
}

async fn call(client: &Client, tool: &str, args: Value) -> Result<Value, String> {
    let params = CallToolRequestParams::new(tool.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    let r = client.call_tool(params).await.map_err(|e| e.to_string())?;
    if r.is_error == Some(true) {
        let text = r
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join(" ");
        return Err(text);
    }
    Ok(r.structured_content.unwrap_or(Value::Null))
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    fs::create_dir_all(p.join("proj/node_modules/pkg")).unwrap();
    fs::write(p.join("proj/package.json"), "{}").unwrap();
    fs::write(p.join("proj/node_modules/pkg/big.js"), vec![b'x'; 300_000]).unwrap();
    fs::write(p.join("video.mp4"), vec![1u8; 200_000]).unwrap();
    fs::write(p.join("copy.mp4"), vec![1u8; 200_000]).unwrap();
    for i in 0..30 {
        fs::write(p.join(format!("small{i:02}.txt")), vec![b'a'; 100 + i]).unwrap();
    }
    dir
}

#[tokio::test]
async fn read_only_session() {
    let data = tempfile::tempdir().unwrap();
    let dir = fixture();
    let root = dir.path().to_string_lossy().into_owned();
    let client = start(&[], data.path()).await;

    let tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(tools.contains(&"scan_directory".to_string()));
    assert!(
        !tools.contains(&"move_to_trash".to_string()),
        "delete must be opt-in"
    );

    let scan = call(&client, "scan_directory", json!({ "path": root }))
        .await
        .unwrap();
    assert!(scan["file_count"].as_u64().unwrap() >= 34);

    // Pagination over 33 top-level entries, 10 at a time.
    let page = call(
        &client,
        "list_children",
        json!({ "path": root, "top": 10, "offset": 10 }),
    )
    .await
    .unwrap();
    assert_eq!(page["children"].as_array().unwrap().len(), 10);
    assert!(page["other_count"].as_u64().unwrap() > 0);

    let files = call(&client, "largest_files", json!({ "path": root, "top": 2 }))
        .await
        .unwrap();
    assert_eq!(files["files"].as_array().unwrap().len(), 2);
    assert_eq!(files["next_offset"], json!(2));

    let sugg = call(
        &client,
        "find_cleanup_candidates",
        json!({ "path": root, "min_size_mib": 0 }),
    )
    .await
    .unwrap();
    assert_eq!(sugg["candidates"][0]["rule"], "node_modules");

    let dupes = call(
        &client,
        "find_duplicates",
        json!({ "path": root, "min_size_mib": 0 }),
    )
    .await
    .unwrap();
    assert!(dupes["total_groups"].as_u64().unwrap() >= 1);

    assert!(
        call(&client, "age_breakdown", json!({ "path": root }))
            .await
            .is_ok()
    );
    assert!(
        call(&client, "extension_breakdown", json!({ "path": root }))
            .await
            .is_ok()
    );
    assert!(call(&client, "list_drives", json!({})).await.is_ok());

    let res = client
        .read_resource(ReadResourceRequestParams::new("cleanmcp://scans"))
        .await
        .unwrap();
    let ResourceContents::TextResourceContents { text, .. } = &res.contents[0] else {
        panic!("expected text resource");
    };
    assert!(text.contains("disk_size"));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn roots_and_delete() {
    let data = tempfile::tempdir().unwrap();
    let dir = fixture();
    let root = dir.path().to_string_lossy().into_owned();
    let client = start(&["--allow-delete", "--root", &root], data.path()).await;

    // Outside the allowed root.
    let parent = dir.path().parent().unwrap().to_string_lossy().into_owned();
    let err = call(&client, "scan_directory", json!({ "path": parent }))
        .await
        .unwrap_err();
    assert!(err.contains("outside"), "{err}");

    // Not shown to the agent yet: refused.
    let video = dir.path().join("video.mp4");
    let r = call(&client, "move_to_trash", json!({ "paths": [video] }))
        .await
        .unwrap();
    assert_eq!(r["results"][0]["ok"], false);
    assert!(video.exists());

    // After the agent has seen it via a tool, it can be trashed.
    let sugg = call(
        &client,
        "find_cleanup_candidates",
        json!({ "path": root, "min_size_mib": 0 }),
    )
    .await
    .unwrap();
    let nm = sugg["candidates"][0]["path"].as_str().unwrap().to_string();
    let r = call(
        &client,
        "move_to_trash",
        json!({ "paths": [nm], "reason": "test" }),
    )
    .await
    .unwrap();
    assert_eq!(r["results"][0]["ok"], true, "{r}");
    assert!(!Path::new(&nm).exists());
    // The cached scan no longer contains it.
    let after = call(
        &client,
        "find_cleanup_candidates",
        json!({ "path": root, "min_size_mib": 0 }),
    )
    .await
    .unwrap();
    assert_eq!(after["total_count"], 0);
    // Listing a folder does not make the folder itself deletable, and roots never are.
    call(&client, "list_children", json!({ "path": root }))
        .await
        .unwrap();
    let r = call(&client, "move_to_trash", json!({ "paths": [root] }))
        .await
        .unwrap();
    assert_eq!(r["results"][0]["ok"], false);
    assert!(dir.path().exists());
    // Every attempt that reached the trash step was audited.
    let log = fs::read_to_string(data.path().join("deletions.log")).unwrap();
    assert_eq!(log.lines().count(), 1);

    client.cancel().await.unwrap();
}

/// A refresh of a subtree must not be shadowed by an older cached scan of its
/// parent: drill-down tools have to see the refreshed data.
#[tokio::test]
async fn refreshed_subtree_wins_over_cached_parent() {
    let data = tempfile::tempdir().unwrap();
    let dir = fixture();
    let root = dir.path().to_string_lossy().into_owned();
    let sub = dir.path().join("proj");
    let sub_s = sub.to_string_lossy().into_owned();
    let client = start(&[], data.path()).await;

    // Cache the parent scan first.
    call(&client, "scan_directory", json!({ "path": root }))
        .await
        .unwrap();
    // New file, then the documented way to get fresh data for a subtree.
    fs::write(sub.join("fresh.txt"), vec![b'z'; 4096]).unwrap();
    call(
        &client,
        "scan_directory",
        json!({ "path": sub_s, "refresh": true }),
    )
    .await
    .unwrap();

    let kids = call(&client, "list_children", json!({ "path": sub_s }))
        .await
        .unwrap();
    let seen = kids["children"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"].as_str().is_some_and(|p| p.ends_with("fresh.txt")));
    assert!(seen, "refreshed subtree was shadowed by the parent: {kids}");

    client.cancel().await.unwrap();
}

/// The summary has to say when totals are partial or truncated.
#[tokio::test]
async fn scan_summary_is_self_describing() {
    let data = tempfile::tempdir().unwrap();
    let dir = fixture();
    let root = dir.path().to_string_lossy().into_owned();
    let client = start(&[], data.path()).await;

    let scan = call(&client, "scan_directory", json!({ "path": root }))
        .await
        .unwrap();
    assert_eq!(scan["one_filesystem"], json!(false));
    assert_eq!(scan["cancelled"], json!(false));
    // 33 top-level entries, 15 listed in 	op_children.
    assert_eq!(scan["other_children_count"], json!(33 - 15));
    assert!(scan["other_children_disk_size"].as_u64().is_some());

    client.cancel().await.unwrap();
}

/// Duplicate groups page like the other list tools do.
#[tokio::test]
async fn duplicate_groups_page() {
    let data = tempfile::tempdir().unwrap();
    let dir = fixture();
    // ixture has one pair; add a second, larger one so two pages exist.
    fs::write(dir.path().join("big-a.bin"), vec![7u8; 400_000]).unwrap();
    fs::write(dir.path().join("big-b.bin"), vec![7u8; 400_000]).unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let client = start(&[], data.path()).await;

    let first = call(
        &client,
        "find_duplicates",
        json!({ "path": root, "min_size_mib": 0, "top": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(first["groups"].as_array().unwrap().len(), 1);
    assert_eq!(first["next_offset"], json!(1));

    let second = call(
        &client,
        "find_duplicates",
        json!({ "path": root, "min_size_mib": 0, "top": 1, "offset": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(second["groups"].as_array().unwrap().len(), 1);
    assert_ne!(second["groups"][0], first["groups"][0]);
    assert_eq!(second["next_offset"], json!(null));

    client.cancel().await.unwrap();
}
