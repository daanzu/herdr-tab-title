# Herdr Tab Title

Automatic tmux-like tab titles for [Herdr](https://herdr.dev).

This plugin keeps Herdr tab labels in sync with the focused pane in each tab:

- foreground program running: `vim`, `cargo`, `node`
- idle shell: current directory basename, such as `herdr` or `api`

It uses only public Herdr plugin and CLI APIs. It does not patch Herdr.

## Install

```bash
herdr plugin install daanzu/herdr-tab-title
herdr plugin action invoke daanzu.tab-title.start
```

On Windows, invoke `daanzu.tab-title.start-windows` instead. Windows uses
PowerShell launchers because Herdr does not reliably run relative executable
paths from action processes.

To stop Herdr asking for a tab name before each new tab, set this in
`~/.config/herdr/config.toml`:

```toml
[ui]
prompt_new_tab_name = false
```

Then reload Herdr config:

```bash
herdr server reload-config
```

## Configuration

Plugin configuration lives in the directory printed by:

```bash
herdr plugin config-dir daanzu.tab-title
```

Create or edit `config.toml` there:

```toml
interval_seconds = 10
directory_depth = 2
show_tab_number = true
idle_label_mode = "shell"
idle_shell_separator = " ❯ "
shorten_home_directory = true
set_window_title = true
append_internal_tab_title = true
windows_process_detection = true
windows_agent_detection = true
```

`interval_seconds` controls the fallback poll. Normal workspace, tab, and pane
events trigger an immediate debounced sync, so titles update without waiting for
the next poll. The default fallback is `10` seconds.

`directory_depth` controls how many trailing path components are shown when a
pane is sitting at an idle shell. The default is `1`, so `/home/me/api` displays
as `api`; `2` displays it as `me/api`. Foreground programs still win, so a pane
running `vim` or `cargo` is titled `vim` or `cargo`.

`idle_label_mode` controls idle-pane labels. It can be `directory` for `me/api`,
`shell` for `bash`, or `directory_shell` for `me/api ❯ bash`. It defaults to
`shell`.

`idle_shell_separator` controls the separator used by `directory_shell`; it
defaults to ` ❯ `.

`shorten_home_directory` displays paths beneath the current user’s home as
`~`, such as `~/project/src`. It defaults to `false`.

`show_tab_number` prefixes tab titles with the visual tab index used by
`prefix+1..9`, such as `1:me/api`. Manual titles keep their text and get the
same prefix. It defaults to `true`.

`set_window_title` updates the title of the foreground terminal client through
Herdr's CLI. It defaults to `true`. By default, `append_internal_tab_title`
includes the focused pane's internal terminal title, producing
`Herdr · <focused tab label> · <internal title>`. Set it to `false` to use only
`Herdr · <focused tab label>`.

`windows_process_detection` enables the Windows process-tree fallback and
defaults to `true`. `windows_agent_detection` enables the Windows fallback to
Herdr's semantic pane `agent` metadata, and also defaults to `true`. The latter
is useful for MSYS-launched agents whose process tree becomes disconnected from
the pane shell. Both options have no effect on Unix platforms.

On Windows, Herdr can report only the pane shell for ordinary foreground
programs. When that happens, the plugin takes one native process snapshot and
walks descendants of Herdr's `shell_pid`, passing through shells and known
launch wrappers. It uses the first program on a branch only when all candidate
branches resolve to the same executable.

MSYS launch wrappers can exit while a native agent such as Pi continues
running, disconnecting the agent from the pane shell's Windows process tree. If
no foreground program can be found, the plugin uses Herdr's semantic `agent`
metadata before falling back to the idle-shell label. A discovered foreground
program still wins over the agent label. These Windows fallbacks do not inspect
or parse terminal titles; ambiguous process trees without agent metadata remain
labeled as idle shells.

## Actions

```bash
herdr plugin action invoke daanzu.tab-title.start
herdr plugin action invoke daanzu.tab-title.stop
herdr plugin action invoke daanzu.tab-title.status
herdr plugin action invoke daanzu.tab-title.sync
```

Windows exposes the same actions with `-windows` suffixes, for example:
`daanzu.tab-title.start-windows`.

The watcher refreshes titles immediately from normal workspace, tab, and pane
events, with a 250ms debounce for event bursts. It also runs a fallback refresh
every `interval_seconds`.

## Manual Tab Names

Manual tab names are respected by default. The plugin manages tabs that still
have Herdr's generated numeric labels, or tabs whose current label matches the
last label the plugin set.

To overwrite manual names for one run:

```bash
bin/herdr-tab-title sync --force
bin/herdr-tab-title start --force
```

On Windows, use `bin\herdr-tab-title.exe` instead.

## Development

On Linux and macOS:

```bash
cargo test
cargo build --release
herdr plugin link .
herdr plugin action invoke daanzu.tab-title.start
```

On Windows, use PowerShell:

```powershell
cargo test
cargo build --release
New-Item -ItemType Directory -Force bin
Copy-Item target/release/herdr-tab-title.exe bin/herdr-tab-title.exe
herdr plugin link .
herdr plugin action invoke daanzu.tab-title.start-windows
```

Local `plugin link` does not run build steps. Run the platform-specific build
script (`scripts/install-binary.sh` or `scripts/install-binary.ps1`) before
linking, or build and copy the binary manually as above. Windows releases use
`herdr-tab-title-x86_64-pc-windows-msvc.exe`.
