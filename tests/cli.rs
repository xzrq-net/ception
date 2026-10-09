//! Integration suite: the `ception` CLI as a subprocess against the fake
//! app-server. Port of test/ception.test.mjs; each test names the JS test it
//! ports, adaptations are commented where they differ.

mod support;

use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use support::*;

// ----- basics ------------------------------------------------------------------

#[test]
fn skill_prints_the_guide_verbatim() {
    let ctx = Ctx::new("happy");
    let guide = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/SKILL.md")).unwrap();
    let out = ctx.ception(&["skill"]).run().expect_code(0);
    assert_eq!(out.stdout, guide);
}

/// Not in the JS suite: the positional-label CLI's prompt contract.
#[test]
fn spawn_reads_a_dash_prompt_from_stdin_and_requires_a_prompt() {
    let ctx = Ctx::new("happy");

    let missing = ctx.ception(&["spawn", "empty"]).run().expect_code(4);
    assert_has(&missing.stderr, "requires a prompt");
    assert_eq!(ctx.fake_state()["appServerStarts"], Value::Null, "no prompt must start nothing");

    ctx.ception(&["spawn", "piped", "-"]).stdin("prompt from stdin\n").run().expect_code(0);
    let state = ctx.fake_state();
    assert_eq!(state["lastTurnStarts"][0]["prompt"].as_str().map(str::trim_end), Some("prompt from stdin"));
}

#[test]
fn happy_path_spawn_renders_report_persists_state_logs_reasoning() {
    let ctx = Ctx::new("happy");

    let out = ctx.ception(&["spawn", "alpha", "do the thing"]).run().expect_code(0);
    let log_lines = out.lines_starting("log: ");
    assert_eq!(log_lines.len(), 1, "{out}");
    assert!(log_lines[0].ends_with("alpha.log"), "{out}");
    assert_has(&out.stdout, "Handled the requested task");
    assert_has(&out.stdout, "files touched:");
    assert_lacks(&out.stdout, "threadId:");

    let record = ctx.record(SESSION, "alpha");
    assert_eq!(record["thread_id"], "thr_1");
    assert_eq!(log_lines[0], format!("log: {}", ctx.log_path(SESSION, "alpha").display()));
    assert_has(&ctx.log(SESSION, "alpha"), "Thinking through fixture");
}

#[test]
fn send_to_idle_live_daemon_starts_a_second_turn_on_the_same_app_server() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["spawn", "idle", "first"]).run().expect_code(0);
    let second = ctx.ception(&["send", "idle", "follow up"]).run().expect_code(0);
    assert!(second.lines_starting("log: ").iter().any(|line| line.ends_with("idle.log")), "{second}");
    assert_has(&second.stdout, "Resumed the prior run");

    let state = ctx.fake_state();
    assert_eq!(state["appServerStarts"], 1);
    let turns = state["lastTurnStarts"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["threadId"], turns[1]["threadId"]);
}

#[test]
fn send_during_active_turn_steers_and_original_client_completes() {
    let ctx = Ctx::new("steer");

    let mut first = ctx.ception(&["spawn", "steer", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    let steered = ctx.ception(&["send", "steer", "add steering"]).timeout(secs(3)).run().expect_code(0);
    assert_has(&steered.stdout, "steered active turn");

    let completed = first.wait().expect_code(0);
    assert_has(&completed.stdout, "Steered response");

    let state = ctx.fake_state();
    let steers = state["steers"].as_array().unwrap();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["prompt"], "add steering");
}

#[test]
fn dead_daemon_plus_stored_thread_respawns_and_resumes() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["spawn", "resume", "first"]).run().expect_code(0);
    ctx.ception(&["kill", "resume"]).run().expect_code(0);

    let sent = ctx.ception(&["send", "resume", "follow up after death"]).run().expect_code(0);
    assert_has(&sent.stdout, "Resumed the prior run");

    let state = ctx.fake_state();
    assert_eq!(state["appServerStarts"], 2);
    assert!(requests(&state, "thread/resume").count() > 0);
}

#[test]
fn server_initiated_request_is_rejected_and_fails_the_turn() {
    let ctx = Ctx::new("server-request");

    let out = ctx.ception(&["spawn", "approval", "needs approval"]).run().expect_code(4);
    assert_has(&out.stderr, "codex sent item/commandExecution/requestApproval");
    assert_has(&ctx.log(SESSION, "approval"), "[error] codex sent item/commandExecution/requestApproval");

    // Adaptation: the client is answered before the rejection and the
    // interrupt reach the fake, so wait for them to land before counting.
    wait_until(WAIT, || {
        let state = ctx.fake_state();
        state["rejectedRequests"].as_u64() >= Some(1) && array_len(&state["interrupts"]) >= 1
    });
    let state = ctx.fake_state();
    assert_eq!(state["rejectedRequests"], 1);
    assert_eq!(array_len(&state["interrupts"]), 1);
}

#[test]
fn spawn_race_yields_one_daemon() {
    let ctx = Ctx::new("happy");

    let codes = std::thread::scope(|scope| {
        let left = scope.spawn(|| ctx.ception(&["spawn", "race", "left"]).run());
        let right = scope.spawn(|| ctx.ception(&["spawn", "race", "right"]).run());
        let mut codes = [left.join().unwrap().code, right.join().unwrap().code];
        codes.sort();
        codes
    });
    assert_eq!(codes, [Some(0), Some(4)]);
    assert_eq!(ctx.fake_state()["appServerStarts"], 1);
}

#[test]
fn failed_turn_maps_to_exit_code_2() {
    let ctx = Ctx::new("fail");
    let out = ctx.ception(&["spawn", "boom", "explode"]).run().expect_code(2);
    assert_has(&out.stdout, "status: failed");
}

#[test]
fn interrupted_turn_maps_to_exit_code_3() {
    let ctx = Ctx::new("steer");

    let mut first = ctx.ception(&["spawn", "stop", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    let interrupted = ctx.ception(&["interrupt", "stop"]).timeout(secs(3)).run().expect_code(0);
    assert_has(&interrupted.stdout, "interrupt requested");

    let completed = first.wait().expect_code(3);
    assert_has(&completed.stdout, "status: interrupted");
}

#[test]
fn daemon_is_reparented_at_spawn_and_survives_spawn_client_death_mid_turn() {
    let ctx = Ctx::new("steer");

    let mut first = ctx.ception(&["spawn", "orphan", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    // The daemon must already be out of the client's process tree.
    let daemon_pid = daemon_pid(&ctx.log(SESSION, "orphan"));
    assert_ne!(ppid(daemon_pid), first.pid());

    first.kill();
    first.wait();

    // The turn is still running daemon-side; steer it to completion.
    let steered = ctx.ception(&["send", "orphan", "add steering"]).timeout(secs(3)).run().expect_code(0);
    assert_has(&steered.stdout, "steered active turn");

    assert!(ctx.wait_status("orphan", "idle", WAIT), "orphan never went idle");
}

/// Adaptation: the JS made the state dir read-only (EACCES). Here the record
/// write is the one that fails while the log works: `<label>.json` is a
/// directory, so renaming the temp file over it fails.
#[test]
fn a_daemon_that_cannot_register_its_thread_shuts_down_instead_of_lingering() {
    let ctx = Ctx::new("happy");
    fs::create_dir_all(ctx.record_path(SESSION, "ghost")).unwrap();

    let out = ctx.ception(&["spawn", "ghost", "hello"]).run().expect_code(4);
    let record = ctx.record_path(SESSION, "ghost");
    assert_has(&out.stderr, &format!("write {}: Is a directory", record.display()));

    assert!(
        wait_until(WAIT, || ctx.sockets().is_empty()),
        "daemon socket still present: daemon lingered after failing to register: {:?}",
        ctx.sockets()
    );
    let lock = ctx.lock_path(SESSION, "ghost");
    assert!(wait_until(WAIT, || lock_free(&lock)), "label lock still held");
}

// ----- watch ---------------------------------------------------------------------

#[test]
fn watch_attaches_to_the_active_turn_and_delivers_its_report() {
    let ctx = Ctx::new("steer");

    let mut first = ctx.ception(&["spawn", "peek", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    let mut watcher = ctx.ception(&["watch", "peek"]).spawn();
    watcher.wait_stdout("log: ");
    std::thread::sleep(ms(200));

    ctx.ception(&["send", "peek", "add steering"]).timeout(secs(3)).run().expect_code(0);

    first.wait().expect_code(0);
    let watched = watcher.wait().expect_code(0);
    assert_has(&watched.stdout, "Steered response");
    assert_has(&watched.stdout, "files touched:");
}

#[test]
fn watcher_of_an_interrupted_turn_exits_3() {
    let ctx = Ctx::new("steer");

    let mut first = ctx.ception(&["spawn", "halt", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    let mut watcher = ctx.ception(&["watch", "halt"]).spawn();
    watcher.wait_stdout("log: ");
    std::thread::sleep(ms(200));

    ctx.ception(&["interrupt", "halt"]).timeout(secs(3)).run().expect_code(0);

    first.wait().expect_code(3);
    let watched = watcher.wait().expect_code(3);
    assert_has(&watched.stdout, "status: interrupted");
}

#[test]
fn daemon_shutdown_settles_attached_watchers_instead_of_hanging() {
    let ctx = Ctx::new("steer");

    let mut first = ctx.ception(&["spawn", "doomed", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    let mut watcher = ctx.ception(&["watch", "doomed"]).spawn();
    watcher.wait_stdout("log: ");
    std::thread::sleep(ms(200));

    ctx.ception(&["kill", "doomed"]).timeout(secs(3)).run().expect_code(0);

    // Interrupt result (3) or shutdown error (4) depending on which settles
    // the socket first; hanging or "completed" are the failures.
    let watched = watcher.wait_timeout(secs(4)).expect("watcher did not settle on daemon shutdown");
    assert_ne!(watched.code, Some(0), "{watched}");
    first.wait();
}

#[test]
fn kill_shuts_the_daemon_down_even_when_the_interrupt_is_rejected() {
    let ctx = Ctx::new("interrupt-reject");

    let mut first = ctx.ception(&["spawn", "stubborn", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    let mut watcher = ctx.ception(&["watch", "stubborn"]).spawn();
    watcher.wait_stdout("log: ");
    std::thread::sleep(ms(200));

    ctx.ception(&["kill", "stubborn"]).timeout(secs(4)).run().expect_code(0);

    assert!(ctx.wait_status("stubborn", "dead", WAIT), "stubborn never died");

    let watched = watcher.wait_timeout(secs(4)).expect("watcher did not settle after rejected interrupt");
    assert_ne!(watched.code, Some(0), "{watched}");
    first.wait();
}

#[test]
fn watch_on_an_idle_daemon_returns_immediately() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["spawn", "quiet", "first"]).run().expect_code(0);
    let watched = ctx.ception(&["watch", "quiet"]).timeout(secs(3)).run().expect_code(0);
    assert_has(&watched.stdout, "no active turn");
}

#[test]
fn watch_without_a_live_daemon_fails_with_exit_4() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["spawn", "gone", "first"]).run().expect_code(0);
    ctx.ception(&["kill", "gone"]).run().expect_code(0);

    let watched = ctx.ception(&["watch", "gone"]).timeout(secs(3)).run().expect_code(4);
    assert_has(&watched.stderr, "no live daemon");
}

// ----- lifetime, sessions, revival ---------------------------------------------------

#[test]
fn respawn_after_daemon_death_preserves_spawn_time_options() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["spawn", "opts", "--model", "gpt-fixture", "--effort", "low", "first"]).run().expect_code(0);
    ctx.ception(&["kill", "opts"]).run().expect_code(0);

    ctx.ception(&["send", "opts", "follow up after death"]).run().expect_code(0);

    let state = ctx.fake_state();
    let last = last_turn_start(&state);
    assert_eq!(last["model"], "gpt-fixture");
    assert_eq!(last["effort"], "low");
    assert_eq!(last["sandboxPolicy"]["type"], "dangerFullAccess");
}

/// Port of "Claude pid watch exits daemon after watched process dies". The
/// label used to become "adoptable"; now it is simply dead.
#[test]
fn watched_process_death_ends_the_daemon() {
    let ctx = Ctx::new("happy");
    let mut dummy = Dummy::start();
    let session = ctx.env.set("CEPTION_SESSION", "watched").set("CEPTION_WATCH_PID", dummy.pid());

    ctx.ception(&["spawn", "watch", "first"]).env(&session).run().expect_code(0);
    assert_eq!(ctx.status("watch").as_deref(), Some("idle"));
    dummy.kill();

    assert!(ctx.wait_status("watch", "dead", secs(5)), "daemon outlived its watched process");
    assert_has(&ctx.log("watched", "watch"), &format!("shutting down: watched process {} exited", dummy.pid()));
}

#[test]
fn two_live_sessions_use_the_same_label_without_collision() {
    let ctx = Ctx::new("happy");
    let dummy_a = Dummy::start();
    let dummy_b = Dummy::start();
    let session_a = ctx.env.set("CEPTION_SESSION", "session-a").set("CEPTION_WATCH_PID", dummy_a.pid());
    let session_b = ctx.env.set("CEPTION_SESSION", "session-b").set("CEPTION_WATCH_PID", dummy_b.pid());

    ctx.ception(&["spawn", "impl", "first"]).env(&session_a).run().expect_code(0);
    ctx.ception(&["spawn", "impl", "first"]).env(&session_b).run().expect_code(0);

    let state = ctx.fake_state();
    assert_eq!(state["appServerStarts"], 2);
    let threads: Vec<Value> =
        state["lastTurnStarts"].as_array().unwrap().iter().map(|turn| turn["threadId"].clone()).collect();
    assert_ne!(threads[0], threads[1]);

    // Follow-up in session A lands on A's thread, not B's.
    ctx.ception(&["send", "impl", "follow up"]).env(&session_a).run().expect_code(0);
    assert_eq!(last_turn_start(&ctx.fake_state())["threadId"], threads[0]);
}

/// Replaces "send refuses a label owned by a live session": adoption is gone,
/// so any other session's label is refused and named.
#[test]
fn send_to_a_label_only_another_session_has_fails_naming_it() {
    let ctx = Ctx::new("happy");
    let session_a = ctx.env.set("CEPTION_SESSION", "session-a");

    ctx.ception(&["spawn", "impl", "first"]).env(&session_a).run().expect_code(0);

    let stolen = ctx.ception(&["send", "impl", "mine now"]).run().expect_code(4);
    assert_has(&stolen.stderr, "belongs to another session");
    assert_has(&stolen.stderr, "session-a");
    assert_eq!(ctx.fake_state()["appServerStarts"], 1);
}

/// Replaces "dead session's label is adopted with thread and options intact":
/// a resumed Claude session keeps its id under a new pid and finds its label.
#[test]
fn a_resumed_session_revives_its_label_with_thread_and_options() {
    let ctx = Ctx::new("happy");
    let mut dummy = Dummy::start();
    let before = ctx.env.set("CEPTION_WATCH_PID", dummy.pid());

    ctx.ception(&["spawn", "impl", "--model", "gpt-fixture", "--effort", "low", "first"])
        .env(&before)
        .run()
        .expect_code(0);
    let thread = ctx.record(SESSION, "impl")["thread_id"].clone();
    dummy.kill();
    assert!(ctx.wait_status("impl", "dead", secs(5)), "daemon outlived its watched process");

    // Same session, new watched process (ctx.env watches the test process).
    let sent = ctx.ception(&["send", "impl", "follow up after death"]).run().expect_code(0);
    assert_has(&sent.stdout, "Resumed the prior run");

    assert_eq!(ctx.record(SESSION, "impl")["thread_id"], thread);
    let state = ctx.fake_state();
    assert_eq!(state["appServerStarts"], 2);
    let resumes: Vec<&Value> = requests(&state, "thread/resume").collect();
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["params"]["threadId"], thread);
    let last = last_turn_start(&state);
    assert_eq!(last["threadId"], thread);
    assert_eq!(last["model"], "gpt-fixture");
    assert_eq!(last["effort"], "low");
}

#[test]
fn kill_all_only_touches_the_calling_sessions_daemons() {
    let ctx = Ctx::new("happy");
    let dummy = Dummy::start();
    let session_a = ctx.env.set("CEPTION_SESSION", "session-a").set("CEPTION_WATCH_PID", dummy.pid());

    ctx.ception(&["spawn", "theirs", "first"]).env(&session_a).run().expect_code(0);
    ctx.ception(&["spawn", "mine", "first"]).run().expect_code(0);

    let killed = ctx.ception(&["kill", "--all"]).run();
    assert_has(&killed.stdout, "killed 1 daemon");

    let rows = ctx.list(&ctx.env);
    assert_eq!(row(&rows, "theirs").unwrap()["status"], "idle");
    assert_eq!(row(&rows, "mine").unwrap()["status"], "dead");
}

#[test]
fn labels_resolve_to_the_project_root_not_the_invocation_subdirectory() {
    let ctx = Ctx::new("happy");
    fs::create_dir(ctx.project.join(".git")).unwrap();
    let sub = ctx.project.join("src/deep");
    fs::create_dir_all(&sub).unwrap();

    ctx.ception(&["spawn", "rooted", "first"]).cwd(&sub).run().expect_code(0);
    ctx.ception(&["send", "rooted", "follow up"]).run().expect_code(0);

    let state = ctx.fake_state();
    assert_eq!(state["appServerStarts"], 1);
    let thread_start = requests(&state, "thread/start").next().unwrap();
    let real = fs::canonicalize(&ctx.project).unwrap();
    assert_eq!(thread_start["params"]["cwd"], real.display().to_string());
    assert_eq!(dir_names(&ctx.projects_dir()), [ctx.project_hash()]);
}

/// Adaptation: stale labels are per-label files aged by mtime; temp debris is
/// `*.tmp` in session dirs.
#[test]
fn gc_drops_stale_labels_of_other_sessions_and_stale_temp_files() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["spawn", "keep", "first"]).run().expect_code(0);

    let dead = ctx.session_dir("dead-session");
    fs::create_dir_all(&dead).unwrap();
    let stale_record = dead.join("stale.json");
    let stale_log = dead.join("stale.log");
    let record = serde_json::json!({ "cwd": ctx.project, "thread_id": "thr_gone" });
    fs::write(&stale_record, record.to_string()).unwrap();
    fs::write(&stale_log, "old\n").unwrap();
    set_age(&stale_record, days(30));
    set_age(&stale_log, days(30));
    // Debris of writes that a SIGKILL cut short.
    let old_temp = dead.join("crashed.json.tmp");
    let fresh_temp = dead.join("writing.json.tmp");
    fs::write(&old_temp, "{}\n").unwrap();
    set_age(&old_temp, days(1));
    fs::write(&fresh_temp, "{}\n").unwrap();

    let rows = ctx.list(&ctx.env);
    assert!(row(&rows, "stale").is_none(), "{rows:#?}");
    assert!(row(&rows, "keep").is_some(), "{rows:#?}");
    assert!(!exists(&stale_record));
    assert!(!exists(&stale_log));
    assert!(!exists(&old_temp));
    assert!(exists(&fresh_temp));
}

// ----- continuations -------------------------------------------------------------------

#[test]
fn a_compacted_turn_that_continues_in_a_new_turn_reports_the_continuations_result() {
    let ctx = Ctx::new("continuation");

    // Without continuation tracking this exits 2 with the bare acknowledgement.
    let out = ctx.ception(&["spawn", "cont", "do the work"]).run().expect_code(0);
    assert_has(&out.stdout, "Continued past the compaction and finished the work.");
    assert_lacks(&out.stdout, "Instructions loaded");
    assert_has(&out.stdout, "compactions: 1");
}

#[test]
fn an_unattended_continuation_turn_is_adopted_so_watch_and_list_can_see_it() {
    let ctx = Ctx::new("continuation");

    // Grace window shorter than the continuation delay: the client is released
    // before codex starts the follow-on turn, exactly as it happened in the wild.
    // The continuation then stays live long enough to observe it from outside.
    let env = ctx
        .env
        .set("CEPTION_CONTINUATION_GRACE_MS", 20)
        .set("CEPTION_FAKE_CONTINUATION_DELAY_MS", 400)
        .set("CEPTION_FAKE_CONTINUATION_RUN_MS", 3000);
    let spawned = ctx.ception(&["spawn", "orphan", "do the work"]).env(&env).run();
    assert_has(&spawned.stdout, "Instructions loaded");

    // Adaptation: after the JS's fixed 600ms, keep polling briefly (the
    // continuation runs 3s), so a loaded machine doesn't miss the start.
    std::thread::sleep(ms(600));
    let row = wait_for(secs(2), || {
        let rows = ctx.list(&env);
        row(&rows, "orphan").filter(|row| row["status"] == "active").cloned()
    })
    .expect("the adopted continuation must show as active in list");

    // watch attaches to the adopted turn and blocks until it completes.
    let watched = ctx.ception(&["watch", "orphan"]).env(&env).run().expect_code(0);
    assert_has(&watched.stdout, "Continued past the compaction and finished the work.");

    let log = fs::read_to_string(row["logPath"].as_str().unwrap()).unwrap();
    assert_has(&log, "adopted an unattended continuation turn");
    assert_has(&log, "Continued past the compaction and finished the work.");
    let unknown_turn_started =
        log.lines().any(|line| line.find("unknown notification").is_some_and(|at| line[at..].contains("turn/started")));
    assert!(!unknown_turn_started, "{log}");
}

// ----- goals -------------------------------------------------------------------------------

#[test]
fn an_active_goal_holds_the_report_across_codexs_self_started_follow_on_turn() {
    let ctx = Ctx::new("goal-continuation");

    // The first physical turn ends with "First half done." while the goal is
    // still active; only the continuation's answer should reach the client.
    let out = ctx.ception(&["spawn", "goal", "do the work"]).run().expect_code(0);
    assert_has(&out.stdout, "Goal continuation finished the work.");
    assert_lacks(&out.stdout, "First half done.");
}

#[test]
fn a_goal_that_stops_being_active_settles_the_turn_without_waiting_out_the_grace_window() {
    let ctx = Ctx::new("happy");

    // happy behaviour emits no goal at all, so nothing should be held back.
    let env = ctx.env.set("CEPTION_GOAL_GRACE_MS", 60000);
    let started = Instant::now();
    ctx.ception(&["spawn", "nogoal", "do the work"]).env(&env).run().expect_code(0);
    assert!(started.elapsed() < secs(8), "a goal-less turn must not pay the continuation grace window");
}

#[test]
fn goal_on_a_fresh_label_starts_a_daemon_and_blocks_on_the_run_codex_starts() {
    let ctx = Ctx::new("happy");

    let out = ctx.ception(&["goal", "audit", "review every module and report findings"]).run().expect_code(0);
    assert!(out.lines_starting("log: ").iter().any(|line| line.ends_with("audit.log")), "{out}");
    assert_has(&out.stdout, "Objective met; work finished.");
    assert_has(&out.stdout, "goal: complete — review every module");

    let state = ctx.fake_state();
    let goal_set = requests(&state, "thread/goal/set").next().unwrap();
    assert_eq!(goal_set["params"]["objective"], "review every module and report findings");
    assert_eq!(goal_set["params"]["status"], "active");
    // No prompt turn was needed: the objective alone drove the work.
    assert_eq!(array_len(&state["lastTurnStarts"]), 0);
}

#[test]
fn a_policy_stopped_goal_reports_the_stop_keeps_the_daemon_and_resumes() {
    let ctx = Ctx::new("goal-stopped");

    // The turn fails and the goal goes to blocked; both facts have to reach the
    // report, along with the command that restarts the run.
    let stopped = ctx.ception(&["goal", "audit", "keep grinding on the objective"]).run().expect_code(2);
    assert_has(&stopped.stdout, "status: failed");
    assert_has(&stopped.stdout, "error code: policyStop");
    assert_has(&stopped.stdout, "goal: blocked");
    assert_has(&stopped.stdout, "ception goal audit --resume");

    assert_has(&ctx.list_text(), "audit\tmine\tidle\tgoal=blocked");

    let resumed = ctx.ception(&["goal", "audit", "--resume"]).run().expect_code(0);
    assert_has(&resumed.stdout, "Objective met; work finished.");
    assert_has(&resumed.stdout, "goal: complete");

    // The whole point of resuming rather than respawning: one app-server across
    // the stop, so codex's background shells and subagents are still there.
    let state = ctx.fake_state();
    assert_eq!(state["appServerStarts"], 1);
    assert_eq!(goal_set_statuses(&state), ["active", "active"]);
}

#[test]
fn goal_pause_stops_codex_starting_more_turns_and_show_reports_the_goal() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["goal", "arc", "the long objective"]).run().expect_code(0);

    let paused = ctx.ception(&["goal", "arc", "--pause"]).run().expect_code(0);
    assert_has(&paused.stdout, "goal: paused");
    assert!(paused.lines_starting("log: ").is_empty(), "{paused}");

    let shown = ctx.ception(&["goal", "arc", "--show"]).run();
    assert_has(&shown.stdout, "goal: paused — the long objective");

    let cleared = ctx.ception(&["goal", "arc", "--clear"]).run();
    assert_has(&cleared.stdout, "goal cleared");
    assert_has(&ctx.ception(&["goal", "arc", "--show"]).run().stdout, "no goal set");
}

#[test]
fn send_steers_the_turn_a_goal_started() {
    let ctx = Ctx::new("steer");

    // "steer" behaviour parks the goal's turn open until something steers it.
    let mut running = ctx.ception(&["goal", "arc", "the long objective"]).spawn();
    ctx.wait_listed("arc\tmine\tactive");

    let steer = ctx.ception(&["send", "arc", "stop polishing the parser"]).run().expect_code(0);
    assert_has(&steer.stdout, "steered active turn");

    let finished = running.wait().expect_code(0);
    assert_has(&finished.stdout, "Steer: stop polishing the parser");
}

#[test]
fn interrupt_pauses_an_active_goal_so_the_run_actually_stops() {
    let ctx = Ctx::new("steer");

    let mut running = ctx.ception(&["goal", "arc", "the long objective"]).spawn();
    ctx.wait_listed("arc\tmine\tactive");

    let interrupted = ctx.ception(&["interrupt", "arc"]).run().expect_code(0);
    assert_has(&interrupted.stdout, "goal: paused");
    running.wait().expect_code(3);

    // Without the pause, codex would start another turn on the freed thread.
    assert_eq!(goal_set_statuses(&ctx.fake_state()), ["active", "paused"]);
}

#[test]
fn a_goal_turn_that_starts_and_fails_in_one_batch_still_reaches_the_client() {
    let ctx = Ctx::new("goal-instant");

    // The fixture writes the turn's whole lifecycle in the same batch of lines
    // as the goal/set response, so the daemon dispatches all of it before its
    // own request await resumes. The client must still get the failure.
    let env = ctx.env.set("CEPTION_GOAL_START_MS", 2000);
    let out = ctx.ception(&["goal", "audit", "the objective"]).env(&env).run().expect_code(2);
    assert_has(&out.stdout, "error code: policyStop");
    assert_has(&out.stdout, "goal: blocked");
    assert_lacks(&out.stdout, "started no turn");

    // The same batch also carries the newer goal status. The reply to goal/set
    // says "active" and arrives later in program order, so a daemon that trusts
    // it keeps advertising a run that has already stopped.
    assert_has(&ctx.list_text(), "goal=blocked");
}

/// Not in the JS suite. Codex snapshots the goal, persists it, then answers:
/// the goal's whole turn, failure and the newer `blocked` status can all land
/// before the reply, which carries the stale `active` snapshot.
fn goal_reply_after_its_failed_turn(behavior: &str) {
    let ctx = Ctx::new(behavior);

    let out = ctx.ception(&["goal", "arc", "the objective"]).run().expect_code(2);
    assert_has(&out.stdout, "Goal turn ran before the reply.");
    assert_has(&out.stdout, "error code: policyStop");
    assert_has(&out.stdout, "goal: blocked");
    assert_lacks(&out.stdout, "started no turn");

    assert_has(&ctx.list_text(), "goal=blocked");
}

/// Plain: the reply and everything before it in one write.
#[test]
fn a_goal_reply_arriving_after_its_failed_turn_in_one_write_reports_the_turn() {
    goal_reply_after_its_failed_turn("goal-response-last");
}

/// Split: each message its own write, 20ms apart.
#[test]
fn a_goal_reply_arriving_after_its_failed_turn_in_split_writes_reports_the_turn() {
    goal_reply_after_its_failed_turn("goal-response-last-split");
}

#[test]
fn a_rejected_goal_reaches_its_client_even_as_another_turn_ends_in_the_same_write() {
    let ctx = Ctx::new("goal-set-error");

    // The fixture ends the running turn and rejects the goal in one write; the
    // goal client must get the rejection, not the other turn's report.
    let mut running = ctx.ception(&["spawn", "arc", "slow work"]).spawn();
    ctx.wait_listed("arc\tmine\tactive");

    let goal = ctx.ception(&["goal", "arc", "the objective"]).run().expect_code(4);
    assert_has(&goal.stderr, "goal rejected by fixture");
    assert_lacks(&goal.stdout, "Handled the requested task");

    // The turn that was already running still reports to the client that started it.
    let spawned = running.wait().expect_code(0);
    assert_has(&spawned.stdout, "Handled the requested task");

    let shown = ctx.ception(&["goal", "arc", "--show"]).run().expect_code(0);
    assert_has(&shown.stdout, "no goal set");
}

#[test]
fn a_goal_set_mid_turn_reports_the_turn_codex_starts_for_it_not_the_running_one() {
    let ctx = Ctx::new("goal-during-turn");

    let mut running = ctx.ception(&["spawn", "arc", "slow work"]).spawn();
    ctx.wait_listed("arc\tmine\tactive");

    let goal = ctx.ception(&["goal", "arc", "the objective"]).run().expect_code(0);
    assert_has(&goal.stdout, "Goal turn finished the work.");
    assert_lacks(&goal.stdout, "Parked turn finished.");
    running.wait().expect_code(0);
}

#[test]
fn a_goal_that_stops_before_starting_a_turn_answers_its_client_at_once() {
    let ctx = Ctx::new("goal-stalls");

    // The goal stops without ever starting a turn; the status change is the
    // answer, not the start clock running out.
    let env = ctx.env.set("CEPTION_GOAL_START_MS", 6000);
    let started = Instant::now();
    let out = ctx.ception(&["goal", "arc", "the objective"]).env(&env).run().expect_code(0);
    assert_has(&out.stdout, "goal: usageLimited");
    assert_has(&out.stdout, "started no turn");
    assert!(started.elapsed() < secs(5), "waited out the start clock instead of using the status");
}

#[test]
fn a_turn_that_ends_before_the_goal_takes_effect_is_not_reported_as_the_goals() {
    let ctx = Ctx::new("goal-late-active");

    // The running turn finishes between the goal/set reply and the goal taking
    // effect; its work predates the objective and must not be reported as it.
    let mut running = ctx.ception(&["spawn", "arc", "slow work"]).spawn();
    ctx.wait_listed("arc\tmine\tactive");

    let env = ctx.env.set("CEPTION_GOAL_START_MS", 3000);
    let goal = ctx.ception(&["goal", "arc", "the objective"]).env(&env).run().expect_code(0);
    assert_has(&goal.stdout, "Continuation after the late goal.");
    assert_lacks(&goal.stdout, "Parked turn finished.");
    running.wait().expect_code(0);
}

#[test]
fn a_goal_met_by_the_turn_already_running_still_reports_to_its_client() {
    let ctx = Ctx::new("goal-in-running-turn");

    // Codex folds a goal set mid-turn into that turn; a client watching only
    // for a fresh turn would time out while the work happens in front of it.
    let mut running = ctx.ception(&["spawn", "arc", "slow work"]).spawn();
    ctx.wait_listed("arc\tmine\tactive");

    let env = ctx.env.set("CEPTION_GOAL_START_MS", 3000);
    let goal = ctx.ception(&["goal", "arc", "the objective"]).env(&env).run().expect_code(0);
    assert_has(&goal.stdout, "Objective met inside the running turn.");
    assert_lacks(&goal.stdout, "started no turn");
    running.wait().expect_code(0);
}

#[test]
fn a_subagent_threads_goal_does_not_settle_this_threads_run() {
    let ctx = Ctx::new("goal-continuation");

    // The fixture completes a foreign thread's goal while our report is held for
    // the continuation. Acting on it would cut the run short at "First half".
    let env = ctx.env.set("CEPTION_FAKE_CONTINUATION_DELAY_MS", 600);
    let out = ctx.ception(&["spawn", "sub", "do the work"]).env(&env).run().expect_code(0);
    assert_has(&out.stdout, "Goal continuation finished the work.");
    assert_lacks(&out.stdout, "First half done.");
}

// ----- quota -----------------------------------------------------------------------------

#[test]
fn quota_reports_a_codex_that_fails_to_start_instead_of_exiting_clean() {
    let ctx = Ctx::new("happy");

    // Closing a never-spawned app-server used to await an exit that never
    // comes, and the CLI fell off the event loop reporting success.
    let out = ctx.ception(&["quota"]).env(&ctx.env.set("CEPTION_CODEX_CMD", " ")).run();
    assert_ne!(out.code, Some(0), "{out}");
    assert_has(&format!("{}{}", out.stderr, out.stdout), "CEPTION_CODEX_CMD is empty");
}

/// Not in the JS suite: the table from the fake's account/rateLimits/read.
#[test]
fn quota_prints_the_rate_limit_table() {
    let ctx = Ctx::new("happy");

    let out = ctx.ception(&["quota"]).run().expect_code(0);
    let line = |label: &str| -> String {
        let found = out.lines_starting(&format!("{label} ")).first().map(|line| line.to_string());
        found.unwrap_or_else(|| panic!("no {label:?} row\n{out}"))
    };
    let primary = line("primary");
    assert_has(&primary, "5h");
    assert_has(&primary, "12% used, resets in");
    let secondary = line("secondary");
    assert_has(&secondary, "7d");
    assert_has(&secondary, "47% used, resets in");
    // codex_spark, under its limitName; only its primary window is reported.
    assert_has(&line("Spark"), "3% used, resets in");
    assert_has(&line("credits"), "12.50");
    assert_eq!(out.stdout.trim_end().lines().count(), 4, "{out}");
}

// ----- session identity from the environment ------------------------------------------------

/// Not in the JS suite: under Claude Code, the session is CLAUDE_CODE_SESSION_ID
/// and the watched process CLAUDE_PID.
#[test]
fn claude_code_env_scopes_the_session_and_ties_the_daemon_to_claude() {
    let ctx = Ctx::new("happy");
    let mut claude = Dummy::start();
    let session = "0b1d8f0e-46c2-4f5c-9b1e-1f2d3c4b5a69";
    let env = ctx
        .env
        .unset("CEPTION_SESSION")
        .unset("CEPTION_WATCH_PID")
        .set("CLAUDE_CODE_SESSION_ID", session)
        .set("CLAUDE_PID", claude.pid());

    ctx.ception(&["spawn", "cc", "first"]).env(&env).run().expect_code(0);
    assert_eq!(ctx.record(session, "cc")["thread_id"], "thr_1");
    let rows = ctx.list(&env);
    assert_eq!(row(&rows, "cc").unwrap()["session"], "mine");
    assert_eq!(row(&rows, "cc").unwrap()["status"], "idle");

    claude.kill();
    assert!(ctx.wait_status("cc", "dead", secs(5)), "daemon outlived CLAUDE_PID");
    assert_has(&ctx.log(session, "cc"), &format!("shutting down: watched process {} exited", claude.pid()));
}

/// Not in the JS suite: other harnesses get the `default` session, no watched
/// process, and the idle timeout as the only end.
#[test]
fn no_session_env_uses_the_default_session_and_ends_on_idle_timeout() {
    let ctx = Ctx::new("happy");
    let env = ctx.env.unset("CEPTION_SESSION").unset("CEPTION_WATCH_PID").set("CEPTION_IDLE_TIMEOUT_SECS", 2);

    ctx.ception(&["spawn", "bare", "first"]).env(&env).run().expect_code(0);
    assert_eq!(ctx.record("default", "bare")["thread_id"], "thr_1");
    assert_eq!(ctx.status("bare").as_deref(), Some("idle"));

    // Watch the run dir, not the socket: every status probe is activity and
    // would keep resetting the idle clock.
    assert!(wait_until(secs(8), || ctx.sockets().is_empty()), "daemon never idled out");
    assert_eq!(ctx.status("bare").as_deref(), Some("dead"));
    assert_has(&ctx.log("default", "bare"), "shutting down: idle timeout");
}

// ----- regressions found in review ----------------------------------------------------------

/// Shutdown used to release the label lock while app-server descendants that
/// ignore SIGTERM lived on, so a successor could overlap the old tools.
#[test]
fn kill_releases_the_label_only_after_app_server_descendants_are_gone() {
    let ctx = Ctx::new("stubborn-child");

    ctx.ception(&["spawn", "shell", "start a tool"]).run().expect_code(0);
    let tool = children(&ctx)[0];
    let _reap = Reap::new(&[tool]);
    assert!(alive(tool), "the fake's tool never ran");

    let killed = ctx.ception(&["kill", "shell"]).run().expect_code(0);
    assert_has(&killed.stdout, "shutdown accepted");

    let lock = ctx.lock_path(SESSION, "shell");
    assert!(wait_until(WAIT, || lock_free(&lock)), "daemon never released the label");
    assert!(!alive(tool), "label released while app-server descendant {tool} still runs");
}

/// A server that stops reading its stdin used to block the daemon on a full
/// pipe: status went unanswered and kill could not reach it.
#[test]
fn a_server_that_stops_reading_cannot_wedge_the_daemon() {
    let ctx = Ctx::new("deaf");

    let mut first = ctx.ception(&["spawn", "deaf", "first"]).spawn();
    ctx.wait_turn_starts(1);
    // 1 MiB of steer, far past a pipe buffer, into a server that reads no more.
    let mut steer = ctx.ception(&["send", "deaf", "-"]).stdin(&"x".repeat(1 << 20)).spawn();
    steer.wait_stdout("log: ");
    std::thread::sleep(ms(300));

    let started = Instant::now();
    let rows = ctx.list(&ctx.env);
    assert!(started.elapsed() < secs(2), "list took {:?}", started.elapsed());
    assert_eq!(row(&rows, "deaf").unwrap()["status"], "active", "{rows:#?}");

    let killed = ctx.ception(&["kill", "deaf"]).run().expect_code(0);
    assert_has(&killed.stdout, "shutdown accepted");
    let lock = ctx.lock_path(SESSION, "deaf");
    assert!(wait_until(secs(8), || lock_free(&lock)), "daemon did not end after kill");
    // Both waiting clients hear about it instead of hanging.
    let steered = steer.wait_timeout(secs(5)).expect("steer client hung");
    assert_ne!(steered.code, Some(0), "{steered}");
    let spawned = first.wait_timeout(secs(5)).expect("spawn client hung");
    assert_ne!(spawned.code, Some(0), "{spawned}");
}

/// The turn/start reply arriving after the run had already continued into a
/// new turn used to point the run back at its first turn, so the
/// continuation's completion was ignored and the client hung.
#[test]
fn a_late_turn_start_reply_does_not_rewind_a_continued_run() {
    let ctx = Ctx::new("late-start-reply");

    let out = ctx.ception(&["spawn", "late", "do the work"]).timeout(secs(10)).run().expect_code(0);
    assert_has(&out.stdout, "Continuation turn finished the work.");
}

/// A server exit went unnoticed while a descendant still held its stdout.
#[test]
fn an_app_server_exit_is_noticed_while_a_descendant_holds_its_stdout() {
    let ctx = Ctx::new("exit-with-child");

    let started = Instant::now();
    let out = ctx.ception(&["spawn", "exiter", "work"]).timeout(secs(10)).run();
    let _reap = Reap::new(&children(&ctx));
    let out = out.expect_code(4);
    assert!(started.elapsed() < secs(5), "took {:?} to notice", started.elapsed());
    assert_has(&out.stderr, "exit 17");

    assert!(ctx.wait_status("exiter", "dead", WAIT), "daemon outlived its app-server");
    let lock = ctx.lock_path(SESSION, "exiter");
    assert!(wait_until(WAIT, || lock_free(&lock)), "daemon never released the label");
    let holder = children(&ctx)[0];
    assert!(!alive(holder), "stdout holder {holder} survived the daemon");
}

/// An interrupt with an active goal used to wait for a pending turn/start
/// reply before pausing the goal, which could start more turns meanwhile.
#[test]
fn interrupt_pauses_an_active_goal_without_waiting_for_a_pending_turn_start() {
    let ctx = Ctx::new("slow-turn-start");

    // The fixture's goal never starts a turn; the waiter gives up quickly.
    let env = ctx.env.set("CEPTION_GOAL_START_MS", 200);
    let goal = ctx.ception(&["goal", "arc", "the objective"]).env(&env).run().expect_code(0);
    assert_has(&goal.stdout, "started no turn");

    let mut sent = ctx.ception(&["send", "arc", "manual turn"]).spawn();
    ctx.wait_turn_starts(1);
    let interrupted = ctx.ception(&["interrupt", "arc"]).run().expect_code(0);
    assert_has(&interrupted.stdout, "goal: paused");
    sent.wait().expect_code(3);

    let state = ctx.fake_state();
    assert_eq!(goal_set_statuses(&state), ["active", "paused"]);
    let paused_at = state["requests"]
        .as_array()
        .unwrap()
        .iter()
        .position(|r| r["method"] == "thread/goal/set" && r["params"]["status"] == "paused")
        .unwrap();
    let mark = state["marks"].as_array().unwrap().iter().find(|m| m["mark"] == "turn/start replied").unwrap();
    let requests_before_reply = mark["requests"].as_u64().unwrap() as usize;
    assert!(
        paused_at < requests_before_reply,
        "goal paused only after the turn/start reply (request #{paused_at}, reply after #{requests_before_reply})"
    );
}

/// GC used to delete any old record temp file, even one a live daemon (the
/// lock holder) may be rewriting.
#[test]
fn gc_keeps_a_stale_record_temp_while_its_label_daemon_lives() {
    let ctx = Ctx::new("happy");

    ctx.ception(&["spawn", "busy", "first"]).run().expect_code(0);
    let live_temp = ctx.session_dir(SESSION).join("busy.json.tmp");
    let dead_temp = ctx.session_dir(SESSION).join("gone.json.tmp");
    for temp in [&live_temp, &dead_temp] {
        fs::write(temp, "{}\n").unwrap();
        set_age(temp, days(1));
    }

    ctx.list(&ctx.env);
    assert!(exists(&live_temp), "gc deleted the temp record of a live daemon");
    assert!(!exists(&dead_temp), "gc kept a stale temp record with no daemon");
}

/// Shutdown used to exit before queued replies reached their sockets: a client
/// waiting on a turn whose server crashed got an empty reply instead of why.
#[test]
fn a_crashed_app_server_reaches_the_waiting_client_with_its_stderr() {
    let ctx = Ctx::new("crash");

    let out = ctx.ception(&["spawn", "crash", "work"]).run().expect_code(4);
    assert_has(&out.stderr, "exit 3");
    assert_has(&out.stderr, "distinctive-crash-7731");
    assert!(ctx.wait_status("crash", "dead", WAIT), "daemon outlived its app-server");
}

// ----- --timeout and watch --run --------------------------------------------------------------

#[test]
fn spawn_timeout_hands_back_a_run_that_watch_run_delivers() {
    let ctx = Ctx::new("steer");

    let started = Instant::now();
    let out = ctx.ception(&["spawn", "park", "--timeout", "1", "slow turn"]).run().expect_code(5);
    let elapsed = started.elapsed();
    assert!(elapsed >= secs(1) && elapsed < secs(5), "gave up after {elapsed:?}");
    let run = still_running_run(&out, "park");
    assert_eq!(ctx.status("park").as_deref(), Some("active"));

    // Blocks on the live run...
    let mut live = ctx.ception(&["watch", "park", "--run", &run]).spawn();
    live.wait_stdout("log: ");
    std::thread::sleep(ms(200));
    ctx.ception(&["send", "park", "add steering"]).run().expect_code(0);
    let watched = live.wait().expect_code(0);
    assert_has(&watched.stdout, "Steered response");

    // ...and once it has settled, hands out the retained report.
    let again = ctx.ception(&["watch", "park", "--run", &run]).timeout(secs(3)).run().expect_code(0);
    assert_has(&again.stdout, "Steered response");
}

#[test]
fn timeout_zero_returns_once_the_turn_runs_and_the_turn_keeps_going() {
    let ctx = Ctx::new("steer");

    let started = Instant::now();
    let out = ctx.ception(&["spawn", "park", "--timeout", "0", "slow turn"]).run().expect_code(5);
    assert!(started.elapsed() < secs(1), "took {:?}", started.elapsed());
    still_running_run(&out, "park");
    assert_eq!(ctx.status("park").as_deref(), Some("active"));
    assert_eq!(ctx.turn_starts(), 1);
}

/// The timeout only counts once the turn is running: here turn/start answers
/// a second late, and even a zero timeout waits for it.
#[test]
fn timeout_waits_for_a_turn_that_is_slow_to_start() {
    let ctx = Ctx::new("slow-turn-start");

    let started = Instant::now();
    let out = ctx.ception(&["spawn", "slow", "--timeout", "0", "work"]).run().expect_code(5);
    assert!(started.elapsed() >= secs(1), "gave up after {:?}, before the turn started", started.elapsed());
    still_running_run(&out, "slow");
    assert_eq!(ctx.status("slow").as_deref(), Some("active"));
}

#[test]
fn a_turn_that_settles_inside_the_timeout_reports_as_usual() {
    let ctx = Ctx::new("fail");

    let out = ctx.ception(&["spawn", "boom", "--timeout", "5", "explode"]).run().expect_code(2);
    assert_has(&out.stdout, "status: failed");
    assert_lacks(&out.stdout, "still running");
}

/// The record write fails between thread start and turn start; even a zero
/// timeout must not turn that into "still running".
#[test]
fn a_spawn_failing_before_its_turn_starts_reports_the_error_despite_the_timeout() {
    let ctx = Ctx::new("happy");
    let record = ctx.record_path(SESSION, "ghost");
    fs::create_dir_all(&record).unwrap();

    let out = ctx.ception(&["spawn", "ghost", "--timeout", "0", "hello"]).run().expect_code(4);
    assert_has(&out.stderr, &format!("write {}: Is a directory", record.display()));
    assert_lacks(&out.stdout, "still running");
}

#[test]
fn a_goal_rejected_before_its_turn_starts_reports_the_error_despite_the_timeout() {
    let ctx = Ctx::new("goal-set-error");

    let mut running = ctx.ception(&["spawn", "arc", "slow work"]).spawn();
    ctx.wait_listed("arc\tmine\tactive");

    let goal = ctx.ception(&["goal", "arc", "--timeout", "0", "the objective"]).run().expect_code(4);
    assert_has(&goal.stderr, "goal rejected by fixture");
    assert_lacks(&goal.stdout, "still running");
    running.wait().expect_code(0);
}

#[test]
fn goal_timeout_hands_back_the_goal_turns_run() {
    let ctx = Ctx::new("steer");

    // "steer" parks the turn the goal starts until something steers it.
    let started = Instant::now();
    let out = ctx.ception(&["goal", "arc", "--timeout", "1", "the long objective"]).run().expect_code(5);
    assert!(started.elapsed() >= secs(1), "gave up after {:?}", started.elapsed());
    let run = still_running_run(&out, "arc");

    ctx.ception(&["send", "arc", "stop polishing the parser"]).run().expect_code(0);
    let watched = ctx.ception(&["watch", "arc", "--run", &run]).run().expect_code(0);
    assert_has(&watched.stdout, "Steer: stop polishing the parser");
}

#[test]
fn watch_run_of_an_unknown_run_fails_with_exit_4() {
    let ctx = Ctx::new("steer");

    let out = ctx.ception(&["spawn", "quiet", "--timeout", "0", "slow turn"]).run().expect_code(5);
    let run = still_running_run(&out, "quiet");
    let (generation, _) = run.split_once('.').unwrap();

    let out =
        ctx.ception(&["watch", "quiet", "--run", &format!("{generation}.999")]).timeout(secs(3)).run().expect_code(4);
    assert_has(&out.stderr, "not retained");
    let out = ctx.ception(&["watch", "quiet", "--run", "999"]).timeout(secs(3)).run().expect_code(4);
    assert_has(&out.stderr, "not from this daemon");
}

#[test]
fn a_run_id_from_before_a_restart_is_refused_not_reused() {
    let ctx = Ctx::new("steer");

    let out = ctx.ception(&["spawn", "again", "--timeout", "0", "slow turn"]).run().expect_code(5);
    let old = still_running_run(&out, "again");
    ctx.ception(&["kill", "again"]).run().expect_code(0);

    // The revived daemon numbers its runs afresh; the old id must not name
    // its new work.
    let out = ctx.ception(&["send", "again", "--timeout", "0", "slow turn"]).run().expect_code(5);
    let new = still_running_run(&out, "again");
    assert_ne!(old, new);
    let out = ctx.ception(&["watch", "again", "--run", &old]).timeout(secs(3)).run().expect_code(4);
    assert_has(&out.stderr, "not from this daemon");
}

#[test]
fn watch_timeout_on_a_parked_turn_hands_back_its_run() {
    let ctx = Ctx::new("steer");

    let mut first = ctx.ception(&["spawn", "park", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);

    let started = Instant::now();
    let out = ctx.ception(&["watch", "park", "--timeout", "1"]).run().expect_code(5);
    assert!(started.elapsed() >= secs(1), "gave up after {:?}", started.elapsed());
    let run = still_running_run(&out, "park");

    // It is the run the spawn client is waiting on.
    ctx.ception(&["send", "park", "add steering"]).run().expect_code(0);
    let spawned = first.wait().expect_code(0);
    assert_has(&spawned.stdout, "Steered response");
    let watched = ctx.ception(&["watch", "park", "--run", &run]).timeout(secs(3)).run().expect_code(0);
    assert_has(&watched.stdout, "Steered response");
}

// ----- regressions found in the second review ------------------------------------------------

/// A SIGKILLed daemon runs no cleanup; its app-server's descendants used to
/// outlive it for good, overlapping whatever the next daemon started.
#[test]
fn a_revived_daemon_first_kills_what_a_sigkilled_predecessor_left_running() {
    let ctx = Ctx::new("stubborn-child");

    ctx.ception(&["spawn", "shell", "start a tool"]).run().expect_code(0);
    let tool = children(&ctx)[0];
    let _reap = Reap::new(&[tool]);
    let daemon = daemon_pid(&ctx.log(SESSION, "shell"));
    unsafe { libc::kill(daemon as libc::pid_t, libc::SIGKILL) };
    let lock = ctx.lock_path(SESSION, "shell");
    assert!(wait_until(WAIT, || lock_free(&lock)), "SIGKILLed daemon still holds the label");
    assert!(alive(tool), "the tool should outlive its SIGKILLed daemon (it ignores SIGTERM)");

    ctx.ception(&["send", "shell", "follow up"]).run().expect_code(0);
    let _reap_new = Reap::new(&children(&ctx));
    assert!(!alive(tool), "the predecessor's tool {tool} survived the revival");

    // Taken down before the new daemon started serving.
    let log = ctx.log(SESSION, "shell");
    let lines: Vec<&str> = log.lines().collect();
    let started = lines.iter().rposition(|line| line.starts_with("[daemon] starting ")).unwrap();
    let killed = lines
        .iter()
        .position(|line| line.contains("a previous daemon left running"))
        .unwrap_or_else(|| panic!("no leftover kill logged:\n{log}"));
    let listening = started + lines[started..].iter().position(|line| line.starts_with("[daemon] listening")).unwrap();
    assert!(started < killed && killed < listening, "{log}");
}

/// A turn codex started by itself used to be taken for the requested one, so
/// a rejected turn/start reported that unrelated turn as success.
#[test]
fn an_unsolicited_turn_does_not_answer_a_rejected_turn_start() {
    let ctx = Ctx::new("unsolicited-then-reject");

    let out = ctx.ception(&["spawn", "reject", "work"]).run().expect_code(4);
    assert_has(&out.stderr, "turn rejected by fixture");
    assert_lacks(&out.stdout, "Unrelated work.");

    // Nor is the unrelated turn "the turn is running".
    let out = ctx.ception(&["spawn", "reject0", "--timeout", "0", "work"]).run().expect_code(4);
    assert_has(&out.stderr, "turn rejected by fixture");
    assert_lacks(&out.stdout, "still running");
}

/// A failed write to the app-server used to be dropped silently, leaving the
/// request (and its client) waiting forever.
#[test]
fn a_server_that_closes_its_stdin_fails_the_turn_promptly() {
    let ctx = Ctx::new("close-stdin");

    for args in [&["spawn", "closed", "work"][..], &["spawn", "closed0", "--timeout", "0", "work"]] {
        let started = Instant::now();
        let out = ctx.ception(args).timeout(secs(10)).run().expect_code(4);
        assert!(started.elapsed() < secs(5), "took {:?}", started.elapsed());
        assert_has(&out.stderr, "write to codex app-server");
    }
}

/// A daemon on its way out used to fail the requests queued on it; now it
/// refuses them and the client revives the label and retries. The send waits
/// behind a held report, then the daemon's watched process dies.
#[test]
fn a_send_refused_by_a_daemon_on_its_way_out_revives_the_label_and_succeeds() {
    let ctx = Ctx::new("continuation");
    let mut dummy = Dummy::start();

    // The compacted turn's report is held for a continuation that won't come
    // while the test runs.
    let first_env = ctx
        .env
        .set("CEPTION_WATCH_PID", dummy.pid())
        .set("CEPTION_CONTINUATION_GRACE_MS", 20000)
        .set("CEPTION_FAKE_CONTINUATION_DELAY_MS", 20000);
    let mut first = ctx.ception(&["spawn", "hold", "do the work"]).env(&first_env).spawn();
    let log = ctx.log_path(SESSION, "hold");
    assert!(
        wait_until(WAIT, || fs::read_to_string(&log).is_ok_and(|log| log.contains("holding the report"))),
        "the report was never held"
    );

    // Same session from a new process; the revived daemon plays it straight.
    let second_env = ctx.env.set("CEPTION_FAKE_BEHAVIOR", "happy");
    let mut sent = ctx.ception(&["send", "hold", "follow up after the handover"]).env(&second_env).spawn();
    sent.wait_stdout("log: ");
    std::thread::sleep(ms(300));
    dummy.kill();

    let sent = sent.wait().expect_code(0);
    assert_has(&sent.stdout, "Resumed the prior run");
    assert_eq!(sent.lines_starting("log: ").len(), 1, "{sent}");
    let state = ctx.fake_state();
    assert_eq!(state["appServerStarts"], 2);
    assert_eq!(requests(&state, "thread/resume").count(), 1);
    // The held client goes down with its daemon.
    assert_ne!(first.wait().code, Some(0));
}

/// A run that ended in an infrastructure failure (here a server request,
/// which fails the turn) used to vanish: `watch --run` said it was unknown.
#[test]
fn a_run_lost_to_an_infrastructure_failure_stays_watchable() {
    let ctx = Ctx::new("late-server-request");

    let out = ctx.ception(&["spawn", "lost", "--timeout", "0", "work"]).run().expect_code(5);
    let run = still_running_run(&out, "lost");
    assert!(
        wait_until(WAIT, || ctx.log(SESSION, "lost").contains("[error] codex sent")),
        "the fixture's approval request never arrived"
    );

    let watched = ctx.ception(&["watch", "lost", "--run", &run]).timeout(secs(3)).run().expect_code(4);
    assert_has(&watched.stderr, "codex sent item/commandExecution/requestApproval");
    assert_lacks(&watched.stderr, "not retained");
}

/// A retained report used to carry the goal as it is now, not as it stood
/// when the run settled.
#[test]
fn a_retained_report_keeps_the_goal_as_it_stood_when_the_run_settled() {
    let ctx = Ctx::new("steer");

    // "steer" parks each goal turn until something steers it.
    let out = ctx.ception(&["goal", "arc", "--timeout", "0", "first objective"]).run().expect_code(5);
    let run = still_running_run(&out, "arc");
    ctx.ception(&["send", "arc", "finish it"]).run().expect_code(0);
    ctx.wait_listed("arc\tmine\tidle\tgoal=complete");

    let out = ctx.ception(&["goal", "arc", "--timeout", "0", "second objective"]).run().expect_code(5);
    still_running_run(&out, "arc");

    let watched = ctx.ception(&["watch", "arc", "--run", &run]).timeout(secs(3)).run().expect_code(0);
    assert_has(&watched.stdout, "goal: complete — first objective");
    assert_lacks(&watched.stdout, "second objective");
}

/// The timeout used to start counting only once the daemon was up, so a slow
/// app-server start was paid on top of it.
#[test]
fn timeout_counts_from_the_start_of_the_command() {
    let ctx = Ctx::new("slow-initialize");

    // initialize answers after 1.5s; the turn then parks.
    let started = Instant::now();
    let out = ctx.ception(&["spawn", "park", "--timeout", "1", "slow turn"]).run().expect_code(5);
    let elapsed = started.elapsed();
    assert!(elapsed >= ms(1500) && elapsed < ms(2200), "took {elapsed:?}");
    still_running_run(&out, "park");
}

// ----- regressions found in the third review -------------------------------------------------

/// Leftover cleanup used to trust a recorded process-group id, which an
/// unrelated group can reuse once the app-server's is gone. Now only
/// processes carrying the label's owner token are touched. The planted
/// `.pgid` file is the old design's record, naming an unrelated group of ours.
#[test]
fn leftover_cleanup_kills_only_processes_carrying_the_owner_token() {
    let ctx = Ctx::new("happy");
    let lock = ctx.lock_path(SESSION, "victim");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();

    let mut bystander = Command::new("sleep").arg("1000").process_group(0).spawn().unwrap();
    let bystander_pid = bystander.id();
    let _reap = Reap::new(&[bystander_pid]);
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let pid_ns = fs::read_link("/proc/self/ns/pid").unwrap();
    let namespace = format!("{}/{}", boot_id.trim(), pid_ns.display());
    fs::write(lock.with_extension("pgid"), format!("{bystander_pid} {namespace}\n")).unwrap();

    // Unique, so concurrent runs of the suite can't see each other's.
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    let token = format!("{:08x}{:024x}", std::process::id(), nanos);
    fs::write(lock.with_extension("owner"), &token).unwrap();
    let leftover = Command::new("sleep").arg("1000").env("CEPTION_OWNER", &token).spawn().unwrap();
    let leftover_pid = leftover.id();
    let _reap_leftover = Reap::new(&[leftover_pid]);
    reap_in_background(leftover);

    ctx.ception(&["spawn", "victim", "work"]).run().expect_code(0);
    assert!(!alive(leftover_pid), "the leftover carrying the owner token survived");
    assert!(alive(bystander_pid), "an unrelated process was killed");
    assert_has(&ctx.log(SESSION, "victim"), "killed 1 process(es) a previous daemon left running");

    let _ = bystander.kill();
    let _ = bystander.wait();
}

/// A turn that ran to completion before its turn/start reply used to be
/// waited on forever, or under --timeout reported as a run that never ends.
#[test]
fn a_turn_finished_before_its_turn_start_reply_is_reported() {
    let ctx = Ctx::new("finished-before-reply");

    let out = ctx.ception(&["spawn", "early", "work"]).timeout(secs(10)).run().expect_code(0);
    assert_has(&out.stdout, "Finished before the reply.");

    let out = ctx.ception(&["spawn", "early0", "--timeout", "0", "work"]).timeout(secs(10)).run().expect_code(0);
    assert_has(&out.stdout, "Finished before the reply.");
    assert_lacks(&out.stdout, "still running");
}

/// Same, with codex starting a parked turn of its own before the reply: the
/// requester still gets its turn's report, and the other turn stays tracked.
#[test]
fn a_turn_finished_before_its_reply_is_reported_while_another_turn_runs() {
    let ctx = Ctx::new("finished-before-reply-parked");

    let out = ctx.ception(&["spawn", "early", "work"]).timeout(secs(10)).run().expect_code(0);
    assert_has(&out.stdout, "Finished before the reply.");
    assert_eq!(ctx.status("early").as_deref(), Some("active"), "the unsolicited turn must stay tracked");
}

/// Connections arriving once shutdown had begun used to sit unanswered until
/// the daemon exited. Now they are refused at once, and a send waits out the
/// old daemon and is served by its successor.
#[test]
fn a_daemon_in_its_shutdown_window_refuses_at_once_and_send_reaches_a_successor() {
    let ctx = Ctx::new("slow-interrupt");

    let mut first = ctx.ception(&["spawn", "slow", "slow turn"]).spawn();
    ctx.wait_turn_starts(1);
    ctx.wait_listed("slow\tmine\tactive");
    // Returns once accepted; the daemon then waits 2s on the fixture's interrupt.
    ctx.ception(&["kill", "slow"]).run().expect_code(0);

    let started = Instant::now();
    assert_eq!(ctx.status("slow").as_deref(), Some("dead"));
    assert!(started.elapsed() < secs(1), "list waited {:?} on a daemon shutting down", started.elapsed());

    let sent = ctx.ception(&["send", "slow", "follow up"]).run().expect_code(0);
    assert_has(&sent.stdout, "Resumed the prior run");
    assert_eq!(ctx.fake_state()["appServerStarts"], 2);
    assert_ne!(first.wait().code, Some(0));
}

/// An interrupt used to wait for a pending turn/start reply even when
/// turn/started had already named the turn.
#[test]
fn interrupt_reaches_a_known_turn_without_waiting_for_its_start_reply() {
    let ctx = Ctx::new("started-before-slow-reply");

    let mut spawned = ctx.ception(&["spawn", "early", "work"]).spawn();
    ctx.wait_turn_starts(1);
    let interrupted = ctx.ception(&["interrupt", "early"]).run().expect_code(0);
    assert_has(&interrupted.stdout, "interrupt requested");
    spawned.wait().expect_code(3);

    let state = ctx.fake_state();
    let interrupt_at =
        state["requests"].as_array().unwrap().iter().position(|r| r["method"] == "turn/interrupt").unwrap();
    let mark = state["marks"].as_array().unwrap().iter().find(|m| m["mark"] == "turn/start replied").unwrap();
    let requests_before_reply = mark["requests"].as_u64().unwrap() as usize;
    assert!(
        interrupt_at < requests_before_reply,
        "interrupt sent only after the turn/start reply (request #{interrupt_at}, reply after #{requests_before_reply})"
    );
}

/// Startup used to go ahead without the owner token on disk; now failing to
/// record it fails startup before any app-server exists.
#[test]
fn a_daemon_that_cannot_record_its_owner_token_fails_before_starting_codex() {
    let ctx = Ctx::new("happy");
    fs::create_dir_all(ctx.lock_path(SESSION, "token").with_extension("owner")).unwrap();

    let out = ctx.ception(&["spawn", "token", "work"]).run().expect_code(4);
    assert_has(&out.stderr, "record the app-server owner token");
    assert_eq!(ctx.fake_state()["appServerStarts"], Value::Null, "an app-server was started");
}

// ----- helpers ---------------------------------------------------------------------------

fn requests<'a>(state: &'a Value, method: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    state["requests"].as_array().into_iter().flatten().filter(move |request| request["method"] == method)
}

fn goal_set_statuses(state: &Value) -> Vec<String> {
    requests(state, "thread/goal/set")
        .map(|request| request["params"]["status"].as_str().unwrap().to_string())
        .collect()
}

fn last_turn_start(state: &Value) -> &Value {
    state["lastTurnStarts"].as_array().and_then(|turns| turns.last()).expect("a turn start")
}

/// The run id from a `--timeout` exit, checking the whole reattach line.
#[track_caller]
fn still_running_run(out: &Output, label: &str) -> String {
    let lines = out.lines_starting("still running: run ");
    assert_eq!(lines.len(), 1, "{out}");
    let run = lines[0]["still running: run ".len()..].split(';').next().unwrap().to_string();
    // `<daemon generation>.<n>`
    let (generation, n) = run.split_once('.').unwrap_or_else(|| panic!("malformed run id in:\n{out}"));
    assert!(generation.len() == 8 && n.parse::<u64>().is_ok(), "{out}");
    assert_eq!(lines[0], format!("still running: run {run}; reattach with `ception watch {label} --run {run}`"));
    run
}

/// The pid in the log's last `[daemon] starting ... pid=N` line.
fn daemon_pid(log: &str) -> u32 {
    let starting = log.lines().rfind(|line| line.starts_with("[daemon] starting ")).expect("starting line");
    starting.rsplit_once(" pid=").expect("pid=").1.parse().unwrap()
}

/// Reap a child as soon as it dies, so it doesn't linger as a zombie.
fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || child.wait());
}

/// Pids of the long-lived processes the fake left behind.
fn children(ctx: &Ctx) -> Vec<u32> {
    let state = ctx.fake_state();
    state["children"].as_array().into_iter().flatten().map(|pid| pid.as_u64().unwrap() as u32).collect()
}

fn array_len(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn days(n: u64) -> Duration {
    secs(n * 86_400)
}
