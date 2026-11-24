//! Install -> uninstall byte-for-byte roundtrip and doctor tests in isolated temp HOME (contracts §3.1 & §10).

use baton_adapters::{
    apply_plan, doctor_checks, hook_config_path, install_plan, uninstall_plan, DoctorEnv,
    InstallOptions, Level, Scope,
};
use baton_proto::{id::now_ms, AgentKind};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn temp_home(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "baton-adapters-rt-{label}-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn pretty(v: &serde_json::Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(v).unwrap())
}

#[test]
fn claude_install_uninstall_roundtrip_is_byte_identical() {
    let home = temp_home("claude");
    let backups = home.join("backups");
    let baton_bin = PathBuf::from("/usr/local/bin/baton");

    let settings_path = hook_config_path(AgentKind::Claude, &home, &Scope::User);
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

    // Realistic existing user config with PostToolUse fix-after-edit hook.
    let original = pretty(&json!({
        "env": { "DISABLE_AUTOUPDATER": "1" },
        "hooks": {
            "PostToolUse": [
                {
                    "hooks": [
                        {
                            "command": "bash $HOME/.claude/hooks/fix-after-edit.sh # @claude-code-global:fix-after-edit",
                            "timeout": 30,
                            "type": "command"
                        }
                    ],
                    "matcher": "Edit|MultiEdit|Write"
                }
            ]
        },
        "model": "fable"
    }));
    fs::write(&settings_path, &original).unwrap();

    let opts = InstallOptions {
        scope: Scope::User,
        dry_run: false,
        baton_bin: baton_bin.clone(),
        agy_permission_heuristic: false,
        register_mcp: false,
    };

    let plan = install_plan(AgentKind::Claude, &home, &opts).unwrap();
    assert_eq!(plan.file_changes.len(), 1);
    let backup_dir = apply_plan(&plan, &backups).unwrap();
    assert!(backup_dir.exists());

    // Idempotent second install yields zero file changes.
    let plan2 = install_plan(AgentKind::Claude, &home, &opts).unwrap();
    assert!(plan2.file_changes.is_empty(), "install must be idempotent");

    // Existing PostToolUse must still be intact alongside newly installed Stop hook.
    let installed_val: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert!(installed_val["hooks"]["PostToolUse"].is_array());
    assert!(installed_val["hooks"]["Stop"].is_array());
    assert!(installed_val["hooks"]["PermissionRequest"].is_array());

    // Uninstall (skip executing real `claude mcp remove` CLI in unit test).
    let mut uplan = uninstall_plan(AgentKind::Claude, &home, &Scope::User, &baton_bin).unwrap();
    assert_eq!(uplan.commands.len(), 1);
    assert_eq!(uplan.commands[0][0], "claude");
    uplan.commands.clear();
    apply_plan(&uplan, &backups).unwrap();

    let after_uninstall = fs::read_to_string(&settings_path).unwrap();
    assert_eq!(
        after_uninstall, original,
        "Claude settings.json must match original byte-for-byte after install -> uninstall"
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn codex_install_uninstall_preserves_notify_and_existing_hooks_byte_for_byte() {
    let home = temp_home("codex");
    let backups = home.join("backups");
    let baton_bin = PathBuf::from("/usr/local/bin/baton");

    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();

    let config_toml_path = codex_dir.join("config.toml");
    let original_toml = r#"model = "gpt-6.2-sol"
notify = [
    "/Users/demo/.codex/computer-use/SkyComputerUseClient",
    "turn-ended",
]

[hooks.state."/tmp/existing:post_tool_use:0:0"]
trusted_hash = "sha256:deadbeef"
"#;
    fs::write(&config_toml_path, original_toml).unwrap();

    let hooks_path = hook_config_path(AgentKind::Codex, &home, &Scope::User);
    let original_hooks = pretty(&json!({
        "hooks": {
            "PostToolUse": [
                {
                    "hooks": [
                        {
                            "command": "bash '/Users/demo/.codex/hooks/fix-after-edit.sh'",
                            "timeout": 30,
                            "type": "command"
                        }
                    ],
                    "matcher": "Edit|MultiEdit|Write"
                }
            ]
        }
    }));
    fs::write(&hooks_path, &original_hooks).unwrap();

    let opts = InstallOptions {
        scope: Scope::User,
        dry_run: false,
        baton_bin: baton_bin.clone(),
        agy_permission_heuristic: false,
        register_mcp: false,
    };

    let plan = install_plan(AgentKind::Codex, &home, &opts).unwrap();
    assert!(
        !plan.notes.is_empty(),
        "Codex plan must include trust-hash note"
    );
    assert_eq!(plan.file_changes.len(), 1);
    assert_eq!(plan.file_changes[0].path, hooks_path);
    apply_plan(&plan, &backups).unwrap();

    // Verify config.toml is untouched byte-for-byte!
    assert_eq!(
        fs::read_to_string(&config_toml_path).unwrap(),
        original_toml
    );

    // Doctor must detect Codex notify coexistence and warn on untrusted new hooks.
    let findings = doctor_checks(&DoctorEnv {
        home: home.clone(),
        path_var: String::new(),
        baton_bin: baton_bin.clone(),
    });
    assert!(findings
        .iter()
        .any(|f| f.id == "codex.notify" && f.level == Level::Ok));
    assert!(findings
        .iter()
        .any(|f| f.id == "codex.hook_trust" && f.level == Level::Warn));

    // Uninstall and assert hooks.json matches byte-for-byte.
    let mut uplan = uninstall_plan(AgentKind::Codex, &home, &Scope::User, &baton_bin).unwrap();
    uplan.commands.clear();
    apply_plan(&uplan, &backups).unwrap();

    assert_eq!(fs::read_to_string(&hooks_path).unwrap(), original_hooks);
    assert_eq!(
        fs::read_to_string(&config_toml_path).unwrap(),
        original_toml
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn agy_install_uninstall_removes_created_file_and_preserves_existing_groups() {
    let home = temp_home("agy");
    let backups = home.join("backups");
    let baton_bin = PathBuf::from("/usr/local/bin/baton");

    // Case 1: hooks.json did not exist -> install creates it, uninstall deletes it cleanly.
    let hooks_path = hook_config_path(AgentKind::Agy, &home, &Scope::User);
    assert!(!hooks_path.exists());

    let opts = InstallOptions {
        scope: Scope::User,
        dry_run: false,
        baton_bin: baton_bin.clone(),
        agy_permission_heuristic: true,
        register_mcp: false,
    };

    let plan = install_plan(AgentKind::Agy, &home, &opts).unwrap();
    apply_plan(&plan, &backups).unwrap();
    assert!(hooks_path.exists());

    let val: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    assert!(val["baton"]["SessionStart"].is_array());
    assert!(val["baton"]["PreInvocation"].is_array());
    assert!(val["baton"]["Stop"].is_array());
    assert!(val["baton"]["PreToolUse"].is_array());
    assert!(val["baton"]["PostToolUse"].is_array());

    let mut uplan = uninstall_plan(AgentKind::Agy, &home, &Scope::User, &baton_bin).unwrap();
    uplan.commands.clear();
    apply_plan(&uplan, &backups).unwrap();
    assert!(
        !hooks_path.exists(),
        "uninstall must delete agy hooks.json when only baton group existed"
    );

    // Case 2: hooks.json already has a user hook group -> byte-for-byte preserved.
    fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
    let original = pretty(&json!({
        "user-lint": {
            "PostToolUse": [
                {
                    "hooks": [
                        {
                            "command": "./lint.sh",
                            "type": "command"
                        }
                    ],
                    "matcher": "edit_file"
                }
            ]
        }
    }));
    fs::write(&hooks_path, &original).unwrap();

    let plan2 = install_plan(AgentKind::Agy, &home, &opts).unwrap();
    apply_plan(&plan2, &backups).unwrap();

    let mut uplan2 = uninstall_plan(AgentKind::Agy, &home, &Scope::User, &baton_bin).unwrap();
    uplan2.commands.clear();
    apply_plan(&uplan2, &backups).unwrap();

    assert_eq!(fs::read_to_string(&hooks_path).unwrap(), original);

    let _ = fs::remove_dir_all(&home);
}
