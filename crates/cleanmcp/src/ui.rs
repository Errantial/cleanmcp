//! Interactive terminal UI (`cleanmcp ui`): browse a scan, mark items, move them to the trash.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Result;
use cleanmcp_core::tree::NO_TIME;
use cleanmcp_core::{Node, NodeKind, Scan, SizeMode};
use compact_str::CompactString;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::output::{human, pct};
use crate::remove::Removal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Size,
    Files,
    /// Oldest first: old data is the usual cleanup target.
    Age,
    Name,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Size => Sort::Files,
            Sort::Files => Sort::Age,
            Sort::Age => Sort::Name,
            Sort::Name => Sort::Size,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Size => "size",
            Sort::Files => "files",
            Sort::Age => "age (oldest first)",
            Sort::Name => "name",
        }
    }
}

/// Case-insensitive name order, equivalent to comparing `to_lowercase()`
/// strings but without allocating one per comparison for ASCII names.
fn cmp_name_ci(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        a.bytes()
            .map(|c| c.to_ascii_lowercase())
            .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
    } else {
        a.to_lowercase().cmp(&b.to_lowercase())
    }
}

pub struct App {
    pub scan: Scan,
    /// Child names from the scan root down to the directory being shown.
    stack: Vec<CompactString>,
    /// Indices into the current directory's children, in display order.
    view: Vec<usize>,
    list: ListState,
    /// First visible row.
    offset: usize,
    pub sort: Sort,
    pub mode: SizeMode,
    pub marked: BTreeSet<PathBuf>,
    pub status: String,
    /// Paths waiting for delete confirmation.
    pub confirm: Option<Vec<PathBuf>>,
    pub help: bool,
    pub quit: bool,
}

impl App {
    pub fn new(scan: Scan, mode: SizeMode) -> Self {
        let mut app = App {
            scan,
            stack: Vec::new(),
            view: Vec::new(),
            list: ListState::default(),
            offset: 0,
            sort: Sort::Size,
            mode,
            marked: BTreeSet::new(),
            status: "Press ? for help".into(),
            confirm: None,
            help: false,
            quit: false,
        };
        app.refresh_view();
        app
    }

    pub fn current(&self) -> &Node {
        let mut n = &self.scan.tree;
        for name in &self.stack {
            match n.children().iter().find(|c| c.name == *name) {
                Some(c) => n = c,
                None => break,
            }
        }
        n
    }

    pub fn current_path(&self) -> PathBuf {
        let mut p = self.scan.root.clone();
        for name in &self.stack {
            p.push(name.as_str());
        }
        p
    }

    pub fn selected(&self) -> Option<&Node> {
        let i = *self.view.get(self.list.selected()?)?;
        self.current().children().get(i)
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selected()
            .map(|n| self.current_path().join(n.name.as_str()))
    }

    /// Recompute display order after navigation, sorting or deletion.
    fn refresh_view(&mut self) {
        let mode = self.mode;
        let sort = self.sort;
        let node = self.current();
        let mut v: Vec<usize> = (0..node.children().len()).collect();
        let c = node.children();
        match sort {
            // The scanner keeps children sorted by size on disk, so the default
            // view needs no sorting at all - even in a folder with 100k entries.
            Sort::Size if mode == SizeMode::Disk => {}
            Sort::Size => v.sort_unstable_by_key(|&i| std::cmp::Reverse(c[i].size_by(mode))),
            Sort::Files => v.sort_unstable_by_key(|&i| std::cmp::Reverse(c[i].file_count())),
            // Unknown times go last, not first.
            Sort::Age => v.sort_unstable_by_key(|&i| (c[i].mtime == NO_TIME, c[i].mtime)),
            // Stable, and without lowercasing every name into a new String.
            Sort::Name => v.sort_by(|&a, &b| cmp_name_ci(&c[a].name, &c[b].name)),
        }
        self.view = v;
        let len = self.view.len();
        let sel = self.list.selected().unwrap_or(0).min(len.saturating_sub(1));
        self.list.select((len > 0).then_some(sel));
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.view.len();
        if len == 0 {
            return;
        }
        let cur = self.list.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len as isize - 1) as usize;
        self.list.select(Some(next));
    }

    fn enter(&mut self) {
        if let Some(n) = self.selected().filter(|n| n.is_dir()) {
            let name = n.name.clone();
            self.stack.push(name);
            self.list.select(Some(0));
            self.refresh_view();
        }
    }

    fn leave(&mut self) {
        if let Some(name) = self.stack.pop() {
            self.refresh_view();
            // Re-select the directory we came from.
            let node = self.current();
            if let Some(pos) = self
                .view
                .iter()
                .position(|&i| node.children()[i].name == name)
            {
                self.list.select(Some(pos));
            }
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(p) = self.selected_path() {
            if !self.marked.remove(&p) {
                self.marked.insert(p);
            }
            self.move_by(1);
        }
    }

    /// Marked paths without any that sit inside another marked folder.
    pub fn marked_top_level(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        // BTreeSet order puts a folder right before its contents.
        for p in &self.marked {
            if !out.last().is_some_and(|last| p.starts_with(last)) {
                out.push(p.clone());
            }
        }
        out
    }

    pub fn marked_size(&self) -> u64 {
        self.marked_top_level()
            .iter()
            .filter_map(|p| self.scan.find(p))
            .map(|n| n.size_by(self.mode))
            .sum()
    }

    /// Handle one key press. `remove` moves paths to the trash; `open` reveals a path.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        remove: &mut dyn FnMut(&[PathBuf]) -> Vec<Removal>,
        open: &mut dyn FnMut(&std::path::Path) -> Result<()>,
    ) {
        if let Some(targets) = self.confirm.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.delete(&targets, remove),
                _ => self.status = "Cancelled.".into(),
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        let page = 15;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.leave(),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.refresh_view();
                self.status = format!("Sorted by {}", self.sort.label());
            }
            KeyCode::Char('a') => {
                self.mode = match self.mode {
                    SizeMode::Disk => SizeMode::Apparent,
                    SizeMode::Apparent => SizeMode::Disk,
                };
                self.refresh_view();
                self.status = match self.mode {
                    SizeMode::Disk => "Showing size on disk".into(),
                    SizeMode::Apparent => "Showing apparent size".into(),
                };
            }
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('u') => {
                self.marked.clear();
                self.status = "Cleared marks.".into();
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                let targets: Vec<PathBuf> = if self.marked.is_empty() {
                    self.selected_path().into_iter().collect()
                } else {
                    self.marked_top_level()
                };
                if !targets.is_empty() {
                    self.confirm = Some(targets);
                }
            }
            KeyCode::Char('o') => {
                let p = self.selected_path().unwrap_or_else(|| self.current_path());
                self.status = match open(&p) {
                    Ok(()) => format!("Opened {}", p.display()),
                    Err(e) => format!("Cannot open: {e}"),
                };
            }
            KeyCode::Char('?') => self.help = true,
            _ => {}
        }
    }

    fn delete(&mut self, targets: &[PathBuf], remove: &mut dyn FnMut(&[PathBuf]) -> Vec<Removal>) {
        let sizes: Vec<u64> = targets
            .iter()
            .map(|p| self.scan.find(p).map_or(0, |n| n.size_by(self.mode)))
            .collect();
        let results = remove(targets);
        let (mut ok, mut freed, mut errors) = (0, 0, Vec::new());
        for (r, size) in results.iter().zip(sizes) {
            if r.ok {
                ok += 1;
                freed += size;
                self.scan.remove(&r.path);
                // Drop the mark and any marks inside it.
                self.marked.retain(|m| !m.starts_with(&r.path));
            } else {
                errors.push(format!(
                    "{}: {}",
                    r.path.display(),
                    r.error.as_deref().unwrap_or("failed")
                ));
            }
        }
        // If the directory we are in was removed, climb to one that still exists.
        while !self.stack.is_empty() && self.scan.find(&self.current_path()).is_none() {
            self.stack.pop();
        }
        self.refresh_view();
        self.status = format!("Moved {ok} item(s), {} to the trash.", human(freed));
        if let Some(first) = errors.first() {
            self.status
                .push_str(&format!(" {} failed: {first}", errors.len()));
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .areas(f.area());

        // Build rows only for the visible window: folders can hold 100k+ entries.
        let height = body.height.saturating_sub(2) as usize;
        let sel = self.list.selected().unwrap_or(0);
        if sel < self.offset {
            self.offset = sel;
        } else if height > 0 && sel >= self.offset + height {
            self.offset = sel + 1 - height;
        }
        self.offset = self
            .offset
            .min(self.view.len().saturating_sub(height.max(1)));

        let node = self.current();
        let total = node.size_by(self.mode);
        let mode = match self.mode {
            SizeMode::Disk => "on disk",
            SizeMode::Apparent => "apparent",
        };
        let head = vec![
            Line::from(vec![Span::styled(
                self.current_path().display().to_string(),
                Style::new().bold(),
            )]),
            Line::from(format!(
                "{} {mode} · {} files · {} dirs · sorted by {}",
                human(total),
                node.file_count(),
                node.dir_count(),
                self.sort.label()
            ))
            .dark_gray(),
        ];
        f.render_widget(Paragraph::new(head), header);

        let base = self.current_path();
        let window = self.view.iter().skip(self.offset).take(height.max(1));
        let items: Vec<ListItem> = window
            .map(|&i| {
                let c = &node.children()[i];
                let size = c.size_by(self.mode);
                let share = pct(size, total);
                let filled = ((share / 10.0).round() as usize).min(10);
                let bar = format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled));
                let marked = self.marked.contains(&base.join(c.name.as_str()));
                let age = c
                    .age()
                    .map(|a| format!("{}d", a.as_secs() / 86_400))
                    .unwrap_or_default();
                let (name, style) = match c.kind() {
                    NodeKind::Dir => (
                        format!("{}/", c.name),
                        Style::new().fg(Color::LightBlue).bold(),
                    ),
                    NodeKind::Link => (format!("{} ->", c.name), Style::new().fg(Color::Cyan)),
                    _ => (c.name.to_string(), Style::new()),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if marked { "▸ " } else { "  " },
                        Style::new().fg(Color::Yellow),
                    ),
                    Span::raw(format!("{:>10} ", human(size))),
                    Span::styled(bar, Style::new().fg(Color::Green)),
                    Span::raw(format!(" {share:>5.1}% {:>8} {age:>6}  ", c.file_count())),
                    Span::styled(name, style),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::new().borders(Borders::TOP | Borders::BOTTOM))
            .highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        let mut state =
            ListState::default().with_selected(self.list.selected().map(|s| s - self.offset));
        f.render_stateful_widget(list, body, &mut state);

        let marked = if self.marked.is_empty() {
            String::new()
        } else {
            format!(
                "{} marked ({}) · ",
                self.marked.len(),
                human(self.marked_size())
            )
        };
        let foot = vec![
            Line::from(format!("{marked}{}", self.status)),
            Line::from(
                "↑↓ move  ⏎/→ open  ←/⌫ up  space mark  d trash  o reveal  s sort  a size mode  ? help  q quit",
            )
            .dark_gray(),
        ];
        f.render_widget(Paragraph::new(foot), footer);

        if let Some(targets) = &self.confirm {
            let total: u64 = targets
                .iter()
                .filter_map(|p| self.scan.find(p))
                .map(|n| n.size_by(self.mode))
                .sum();
            let mut lines = vec![
                Line::from(format!(
                    "Move {} item(s), {}, to the trash?",
                    targets.len(),
                    human(total)
                ))
                .bold(),
                Line::from(""),
            ];
            lines.extend(
                targets
                    .iter()
                    .take(8)
                    .map(|p| Line::from(p.display().to_string())),
            );
            if targets.len() > 8 {
                lines.push(Line::from(format!("... and {} more", targets.len() - 8)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from("y = yes, any other key = cancel").dark_gray());
            popup(f, "Confirm", lines);
        } else if self.help {
            let lines = [
                "↑/↓ j/k        move          PgUp/PgDn, g/G  jump",
                "Enter/→/l      open folder   ←/Backspace/h   go up",
                "space          mark/unmark   u               clear marks",
                "d / Delete     move marked (or selected) items to the trash",
                "o              show in Explorer / file manager",
                "s              sort: size, files, age, name",
                "a              size on disk / apparent size",
                "q / Esc        quit",
                "",
                "Nothing is deleted permanently. System folders and your home folder are refused.",
            ];
            popup(f, "Help", lines.iter().map(|l| Line::from(*l)).collect());
        }
    }
}

fn popup(f: &mut Frame, title: &str, lines: Vec<Line>) {
    let area = f.area();
    let w = area.width.saturating_sub(8).min(90);
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" {title} "))),
        rect,
    );
}

/// Show `path` in Explorer (selected) or open its folder in the Linux file manager.
pub fn reveal(path: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer does not understand a quoted "/select,<path>"; quote only the path.
        std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let mut child = std::process::Command::new("xdg-open")
            .arg(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        // Reap the child so it does not linger as a zombie.
        std::thread::spawn(move || child.wait());
    }
    Ok(())
}

/// Run the UI until the user quits.
pub fn run(scan: Scan, mode: SizeMode) -> Result<()> {
    let mut app = App::new(scan, mode);
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|f| app.draw(f))?;
            if let Event::Key(key) = event::read()? {
                // Windows also reports key releases; act on presses only.
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key, &mut crate::remove::move_to_trash, &mut reveal);
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cleanmcp_core::{ScanOptions, scan};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::fs;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn setup() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("big/inner")).unwrap();
        fs::write(p.join("big/inner/blob"), vec![1u8; 200_000]).unwrap();
        fs::write(p.join("big/note.txt"), vec![1u8; 100]).unwrap();
        fs::write(p.join("small.txt"), vec![1u8; 50_000]).unwrap();
        let s = scan(p, &ScanOptions::default()).unwrap();
        (dir, App::new(s, SizeMode::Apparent))
    }

    #[test]
    fn navigate_mark_and_delete() {
        let (_dir, mut app) = setup();
        let mut removed: Vec<PathBuf> = Vec::new();
        let mut remove = |ps: &[PathBuf]| {
            removed.extend_from_slice(ps);
            ps.iter()
                .map(|p| Removal {
                    path: p.clone(),
                    ok: true,
                    error: None,
                })
                .collect()
        };
        let mut open = |_: &std::path::Path| Ok(());

        assert_eq!(&*app.selected().unwrap().name, "big");
        app.handle_key(key(KeyCode::Enter), &mut remove, &mut open);
        assert!(app.current_path().ends_with("big"));
        app.handle_key(key(KeyCode::Char(' ')), &mut remove, &mut open); // mark inner/
        assert_eq!(app.marked.len(), 1);
        assert_eq!(app.marked_size(), 200_000);
        app.handle_key(key(KeyCode::Char('d')), &mut remove, &mut open);
        assert!(app.confirm.is_some());
        app.handle_key(key(KeyCode::Char('y')), &mut remove, &mut open);
        drop(remove);
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("inner"));
        assert!(app.marked.is_empty());
        assert_eq!(app.scan.tree.size, 50_100);
        app.handle_key(key(KeyCode::Left), &mut |_| vec![], &mut open);
        assert_eq!(&*app.selected().unwrap().name, "big");
    }

    #[test]
    fn nested_marks_count_once() {
        let (_dir, mut app) = setup();
        let mut remove = |ps: &[PathBuf]| {
            assert_eq!(ps.len(), 1, "inner mark must be folded into its parent");
            ps.iter()
                .map(|p| Removal {
                    path: p.clone(),
                    ok: true,
                    error: None,
                })
                .collect()
        };
        let mut open = |_: &std::path::Path| Ok(());
        app.handle_key(key(KeyCode::Char(' ')), &mut remove, &mut open); // mark big/
        app.handle_key(key(KeyCode::Up), &mut remove, &mut open);
        app.handle_key(key(KeyCode::Enter), &mut remove, &mut open);
        app.handle_key(key(KeyCode::Char(' ')), &mut remove, &mut open); // mark big/inner
        assert_eq!(app.marked.len(), 2);
        assert_eq!(app.marked_size(), 200_100);
        app.handle_key(key(KeyCode::Char('d')), &mut remove, &mut open);
        app.handle_key(key(KeyCode::Char('y')), &mut remove, &mut open);
        assert!(app.marked.is_empty());
        assert_eq!(app.scan.tree.size, 50_000);
    }

    #[test]
    fn cancel_and_sort() {
        let (_dir, mut app) = setup();
        let mut remove = |_: &[PathBuf]| -> Vec<Removal> { panic!("must not delete") };
        let mut open = |_: &std::path::Path| Ok(());
        app.handle_key(key(KeyCode::Char('d')), &mut remove, &mut open);
        app.handle_key(key(KeyCode::Char('n')), &mut remove, &mut open);
        assert!(app.confirm.is_none());
        app.handle_key(key(KeyCode::Char('s')), &mut remove, &mut open); // files
        app.handle_key(key(KeyCode::Char('s')), &mut remove, &mut open); // age
        app.handle_key(key(KeyCode::Char('s')), &mut remove, &mut open); // name
        assert_eq!(app.sort, Sort::Name);
        assert_eq!(&*app.selected().unwrap().name, "big");
        app.handle_key(key(KeyCode::Down), &mut remove, &mut open);
        assert_eq!(&*app.selected().unwrap().name, "small.txt");
    }

    #[test]
    fn scrolls_to_selection() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..60 {
            fs::write(dir.path().join(format!("f{i:02}")), vec![1u8; 1000 + i]).unwrap();
        }
        let s = scan(dir.path(), &ScanOptions::default()).unwrap();
        let mut app = App::new(s, SizeMode::Apparent);
        app.handle_key(key(KeyCode::End), &mut |_| vec![], &mut |_| Ok(()));
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        // Smallest file is last in size order and must be on screen.
        assert!(text.contains("f00"), "selected row not visible");
        assert!(!text.contains("f59"));
    }

    #[test]
    fn renders_rows_and_dialog() {
        let (_dir, mut app) = setup();
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("big/"));
        assert!(text.contains("small.txt"));
        app.confirm = Some(vec![app.current_path().join("small.txt")]);
        term.draw(|f| app.draw(f)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("to the trash?"));
    }
}
