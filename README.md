# Zed GDScript Extension

This extension adds support for [GDScript](https://docs.godotengine.org/en/stable/classes/index.html), the scripting language used in the Godot game engine, to the Zed editor.

## Requirements

- Zed Editor.
- Godot Engine 4.x, installed anywhere. There is no netcat, no `PATH` entry and
  no manual step required.

## How it works

Godot's GDScript language server is not a program: it lives inside the Godot
editor and speaks TCP, while Zed speaks stdio and expects one server per
worktree. This extension closes that gap with a small supervisor,
[`godot-lsp-bridge`](bridge/), that

- starts its own Godot (`--headless --editor`) for the project the worktree
  names, on a port nobody else uses, so two projects can never answer for each
  other;
- restarts it when it dies, replaying the session, so an engine crash does not
  mean restarting anything by hand;
- reports *why* it cannot start one -- no Godot found, a project that fails to
  load, a port it cannot open -- instead of silently doing nothing.

Because the engine is a child of the bridge, closing the worktree closes it.

## How to install

1. Open Zed.
2. Go to Extensions.
3. Search for *GDScript*.
4. Click Install.

The bridge binary is downloaded for your platform on first use, so there is
nothing else to install.

### macOS

macOS has no prebuilt binary: GitHub's arm64 runners queue for too long, so the
release skips the platform. Build the bridge once instead -- no `--target` is
needed, since a plain `cargo build` produces a binary for whichever machine
(Intel or Apple Silicon) runs it:

```sh
cargo build --release --manifest-path bridge/Cargo.toml
```

Then point the extension at the result, and it stops looking for a download:

```json
{
  "lsp": {
    "gdscript": {
      "binary": {
        "path": "/Users/you/src/zed-gdscript/bridge/target/release/godot-lsp-bridge"
      }
    }
  }
}
```

## Configuration

Out of the box the bridge finds Godot as `godot4`, `godot` or `Godot` on your
`PATH`. To point it at a specific engine, set `GODOT_BIN` for the language
server:

```json
{
  "lsp": {
    "gdscript": {
      "binary": {
        "env": {
          "GODOT_BIN": "C:/Program Files/Godot/Godot_v4.7.2-stable_win64.exe"
        }
      }
    }
  }
}
```

Arguments are passed straight to the bridge -- `--godot PATH` is the same
setting, `--project DIR` overrides the worktree root:

```json
{
  "lsp": {
    "gdscript": {
      "binary": {
        "arguments": ["--godot", "/usr/bin/godot4", "--project", "./game"]
      }
    }
  }
}
```

`--project DIR` is rarely needed: when the worktree root has no `project.godot`
the bridge looks up to three levels below it, skipping whatever `.gitignore`
excludes, and runs the closest project it finds. A monorepo with the game in
`game/`, `examples/demo/` or `crates/client/` needs no configuration. Two
projects at the same depth are reported rather than guessed between.

`binary.path` replaces the downloaded bridge with your own build, which is what
the dev workflow uses:

```json
{
  "lsp": {
    "gdscript": {
      "binary": {
        "path": "C:/src/zed-gdscript/bridge/target/release/godot-lsp-bridge.exe"
      }
    }
  }
}
```

If the engine does not understand `--lsp-port`, the bridge reports what Godot
printed and gives up, rather than connecting to whatever happens to be on the
default port.

### Opening GDScript files in Zed instead of Godot

If you want to open GDScript files in Zed instead of Godot, you need to change Godot editor settings. In the Godot editor, go to *Editor > Editor Settings > Text Editor > External* and enable *Use External Editor*. Set the *Exec Path* to the path of your Zed executable and the *Exec Flags* to `{project} {file}:{line}:{col}`. This will open files and jump to the correct line in Zed when you click on an error or warning in Godot.

If you have installed Godot and Zed via Flatpak on Linux, use the following parameters:

1. Set "Exec Path" to `flatpak-spawn`.
2. Set "Exec Flags" to `--host flatpak run dev.zed.Zed {project} {file}:{line}:{col}`.

## Opening documentation in the Godot editor

You can configure Godot to open the relevant documentation page when running the `editor: go to declaration` command on a built-in piece of code.

First, in Godot, open the Editor Settings. Then, go to Network -> Language Server and turn on Show Native Symbols in Editor. This configures the language server to open documentation pages in the Godot editor on demand.

From there, in Zed, you can use the command `editor: go to declaration` to open the documentation in the editor. Note that it will **not** work with the `editor: go to definition` command (last tested in Godot 4.6.1).

## Formatting

Zed provides the ability to format files using an external formatter. There are a few options for GDScript formatting.

### With [GDScript formatter](https://github.com/GDQuest/GDScript-formatter)

[GDScript formatter](https://github.com/GDQuest/GDScript-formatter) is a fast code formatter and linter that is built with the same [tree-sitter-gdscript](https://github.com/PrestonKnopp/tree-sitter-gdscript) grammar that this extension uses.

Install it and make sure that `gdscript-formatter` is available in your system PATH.

Then, add the following configuration to your `settings.json` file:

```json
{
  "languages": {
    "GDScript": {
      "formatter": {
        "external": {
          "command": "gdscript-formatter",
          "arguments": []
        }
      }
    }
  }
}
```

### With [GDScript Toolkit](https://github.com/Scony/godot-gdscript-toolkit)

[GDScript Toolkit](https://github.com/Scony/godot-gdscript-toolkit) is a Python-based formatter and linter for GDScript.

Install it and make sure that `gdformat` is available in your system PATH.

Then, add the following configuration to your `settings.json` file:

```json
{
  "languages": {
    "GDScript": {
      "formatter": {
        "external": {
          "command": "gdformat",
          "arguments": ["-"]
        }
      }
    }
  }
}
```

## Debugging

> [!NOTE]
> At the time of writing this, debugging will not work when using Godot 4.4. It's confirmed to work in Godot 4.3 and 4.5, however.

The extension provides a debug adapter that allows debugging GDScript code. To be able to do this, create `.zed/debug.json` file in the root of your workspace with the following content:

```json
[
  {
    "adapter": "godot",
    "label": "Godot (Launch)",
    "request": "launch"
  },
  {
    "adapter": "godot",
    "label": "Godot (Attach)",
    "request": "attach"
  }
]
```

This will add 2 debug tasks: `Godot (Launch)` will launch a new debugging session, while `Godot (Attach)` will try to attach to an existing one. To run these tasks, press `F4`, then select the desired task from the menu.

Complete list of options can be found in [`debug_adapter_schemas/godot.json`](debug_adapter_schemas/godot.json). Zed will also suggest possible options through autocompletion when editing the `debug.json` file.

## Contributing

- [Contributing to Scheme files](docs/contributing_scheme_files.md)
