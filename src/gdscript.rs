use std::net::Ipv4Addr;

use zed_extension_api::{
    self as zed, serde_json, serde_json::Value, DebugAdapterBinary, DownloadedFileType,
    GithubReleaseOptions, Result, StartDebuggingRequestArguments,
    StartDebuggingRequestArgumentsRequest, TcpArguments,
};

/// Where the bridge binaries are published, one asset per platform.
const BRIDGE_REPOSITORY: &str = "sdgoij/zed-gdscript";

/// Godot's language server is part of the editor and speaks TCP, never stdio,
/// so the command Zed starts is a supervisor that owns a Godot process. The
/// supervisor is what makes this work without `nc`, keeps a private port per
/// worktree and restarts the engine when it dies.
struct GDScriptExtension {
    /// The bridge once this session has it: restarting a server must not mean
    /// downloading it again.
    bridge: Option<String>,
}

impl zed::Extension for GDScriptExtension {
    fn new() -> Self {
        Self { bridge: None }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary = zed::settings::LspSettings::for_worktree("gdscript", worktree)
            .ok()
            .and_then(|settings| settings.binary);

        let (configured, arguments, configured_env) = match binary {
            Some(binary) => (
                binary.path,
                binary.arguments.unwrap_or_default(),
                binary.env.unwrap_or_default(),
            ),
            None => (None, Vec::new(), Default::default()),
        };

        // A path in the settings is how you point at a locally built bridge.
        // Otherwise the published one is fetched, so nothing has to be found on
        // the PATH and missing pieces are reported instead of silently killing
        // the language server.
        let command = match configured {
            Some(path) => path,
            None => self.ensure_bridge(language_server_id)?,
        };

        // Godot itself is resolved by the bridge, in the order `--godot`, then
        // `GODOT_BIN`, then the PATH, and it says which of them failed.
        //
        // The PATH it inherits is Zed's, which is not necessarily the one a
        // terminal sees: a Zed launched from a desktop entry keeps whatever
        // environment existed when it started, so a directory added to PATH
        // afterwards -- or only exported in a shell profile on Windows -- is
        // invisible. `shell_env` is the login shell's environment, which is the
        // one that knows where Godot is. Explicit `binary.env` entries still win.
        let mut env = worktree.shell_env();
        for (key, value) in configured_env {
            env.retain(|(existing, _)| existing != &key);
            env.push((key, value));
        }

        Ok(zed::Command {
            command,
            args: arguments,
            env,
        })
    }

    fn get_dap_binary(
        &mut self,
        adapter_name: String,
        config: zed::DebugTaskDefinition,
        _user_provided_debug_adapter_path: Option<String>,
        _worktree: &zed::Worktree,
    ) -> Result<DebugAdapterBinary, String> {
        let configuration = serde_json::from_str::<serde_json::Value>(&config.config)
            .map_err(|e| format!("Error parsing the config: {e}"))?;

        let request = self.dap_request_kind(adapter_name, configuration.clone())?;

        let host = configuration
            .get("host")
            .and_then(|host| host.as_str().and_then(|host| host.parse().ok()))
            .unwrap_or(Ipv4Addr::new(127, 0, 0, 1))
            .to_bits();

        let port = configuration
            .get("port")
            .and_then(|port| port.as_u64())
            .unwrap_or(6006) as u16;

        Ok(DebugAdapterBinary {
            command: None,
            arguments: vec![],
            envs: Default::default(),
            cwd: None,

            connection: Some(TcpArguments {
                host,
                port,
                timeout: None,
            }),

            request_args: StartDebuggingRequestArguments {
                configuration: config.config,
                request,
            },
        })
    }

    fn dap_request_kind(
        &mut self,
        _adapter_name: String,
        value: Value,
    ) -> Result<StartDebuggingRequestArgumentsRequest, String> {
        value
            .get("request")
            .and_then(|v| v.as_str())
            .ok_or("`request` was not specified".into())
            .and_then(|request| match request {
                "launch" => Ok(StartDebuggingRequestArgumentsRequest::Launch),
                "attach" => Ok(StartDebuggingRequestArgumentsRequest::Attach),
                _ => Err("`request` must be either `launch` or `attach`".into()),
            })
    }
}

impl GDScriptExtension {
    /// The bridge for this platform, downloaded from the release on first use.
    fn ensure_bridge(&mut self, language_server_id: &zed::LanguageServerId) -> Result<String> {
        if let Some(bridge) = &self.bridge {
            return Ok(bridge.clone());
        }

        let asset = bridge_asset_name()?;
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );

        let bridge = download_bridge(&asset).map_err(|error| {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Failed(error.clone()),
            );
            error
        })?;

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::None,
        );
        self.bridge = Some(bridge.clone());
        Ok(bridge)
    }
}

fn bridge_asset_name() -> Result<String, String> {
    let (os, architecture) = zed::current_platform();
    let target = match (os, architecture) {
        (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc.exe",
        (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
        // No macOS asset is published; `binary.path` is the escape hatch.
        (zed::Os::Mac, _) => {
            return Err(
                "No prebuilt godot-lsp-bridge is published for macOS. Build one with \
                 `cargo build --release --manifest-path bridge/Cargo.toml` and point \
                 `binary.path` in the `gdscript` LSP settings at it."
                    .into(),
            )
        }
        (os, architecture) => {
            return Err(format!(
                "There is no GDScript language server for {os:?} on {architecture:?}."
            ))
        }
    };
    Ok(format!("godot-lsp-bridge-{target}"))
}

fn download_bridge(asset_name: &str) -> Result<String, String> {
    let release = zed::latest_github_release(
        BRIDGE_REPOSITORY,
        GithubReleaseOptions {
            require_assets: true,
            pre_release: false,
        },
    )?;

    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| {
            let available = release
                .assets
                .iter()
                .map(|asset| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Release {} of {BRIDGE_REPOSITORY} has no {asset_name} (it has: {available}). \
                 To use a local build instead, put its path in `lsp.gdscript.binary.path`.",
                release.version
            )
        })?;

    zed::download_file(
        &asset.download_url,
        asset_name,
        DownloadedFileType::Uncompressed,
    )?;
    if zed::current_platform().0 != zed::Os::Windows {
        zed::make_file_executable(asset_name)?;
    }

    Ok(asset_name.to_string())
}

zed::register_extension!(GDScriptExtension);
