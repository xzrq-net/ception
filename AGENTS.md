Tool for Claude Code (and other agent harnesses) to operate Codex as a
subagent.

This is a vibe coded tool. Agents are authorized to make local commits.

`SKILL.md` is the guide for the agent operating ception (`ception skill`
prints it); `README.md` is for humans.

Rust, Linux only. Build and test inside the dev shell:
`nix develop -c cargo test` (the cargo on the system PATH has no linker).
`tests/cli.rs` drives the real binary against the fake app-server in
`src/bin/ception-fake-appserver.rs`; new daemon behavior gets a fake behavior
and an integration test. `nix build` runs the same suite in the sandbox.
