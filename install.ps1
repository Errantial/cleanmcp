# Install the latest cleanmcp release on Windows (x64).
#   irm https://raw.githubusercontent.com/Errantial/cleanmcp/main/install.ps1 | iex
# Options (environment): CLEANMCP_VERSION=v0.1.0, CLEANMCP_INSTALL_DIR=C:\Tools\cleanmcp
#
# Runs in its own scope so `irm | iex` does not change settings in your session.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'

    $repo = 'Errantial/cleanmcp'
    $tag = $env:CLEANMCP_VERSION
    if (-not $tag) { $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name }
    $tag = 'v' + $tag.TrimStart('v')
    $name = "cleanmcp-$($tag.Substring(1))-x86_64-pc-windows-msvc"
    $base = "https://github.com/$repo/releases/download/$tag"
    $dest = $env:CLEANMCP_INSTALL_DIR
    if (-not $dest) { $dest = Join-Path $env:LOCALAPPDATA 'Programs\cleanmcp' }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $name..."
        $zip = Join-Path $tmp "$name.zip"
        Invoke-WebRequest "$base/$name.zip" -OutFile $zip -UseBasicParsing
        Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') -UseBasicParsing
        $line = Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match "\s$([regex]::Escape("$name.zip"))$" }
        $expected = ($line -split '\s+')[0]
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if (-not $expected -or $actual -ne $expected) { throw "Checksum mismatch for $name.zip" }

        Expand-Archive $zip -DestinationPath $tmp -Force
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        $exe = Join-Path $dest 'cleanmcp.exe'
        if (Test-Path $exe) {
            # A running exe (e.g. an MCP client keeping `cleanmcp mcp` open) cannot be
            # overwritten, but it can be renamed. The old copy is removed next time.
            $old = "$exe.old"
            Remove-Item $old -Force -ErrorAction SilentlyContinue
            Move-Item $exe $old -Force
        }
        Copy-Item (Join-Path $tmp "$name\cleanmcp.exe") $exe -Force
        Remove-Item "$exe.old" -Force -ErrorAction SilentlyContinue
        Write-Host "Installed $(& $exe --version) to $dest"

        # Edit the raw registry value so entries like %USERPROFILE%\bin stay unexpanded.
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
        try {
            $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $entries = $raw -split ';' | Where-Object { $_ }
            if ($entries -notcontains $dest) {
                $key.SetValue('Path', (($entries + $dest) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
                # Tell Explorer and new terminals that the environment changed.
                [Environment]::SetEnvironmentVariable('CLEANMCP_INSTALL_REFRESH', '1', 'User')
                [Environment]::SetEnvironmentVariable('CLEANMCP_INSTALL_REFRESH', $null, 'User')
                Write-Host "Added $dest to your user PATH. Open a new terminal to use 'cleanmcp'."
            }
        }
        finally {
            $key.Close()
        }
    }
    finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
