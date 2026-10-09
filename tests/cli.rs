//! Integration suite: the `ception` CLI as a subprocess against the fake
//! app-server. Port of test/ception.test.mjs; each test names the JS test it
//! ports, adaptations are commented where they differ.

mod support;

use std::fs;
use std::time::{Duration, Instant};

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
    let log = ctx.log(SESSION, "orphan");
    let starting = log.lines().find(|line| line.starts_with("[daemon] starting ")).expect("starting line");
    let daemon_pid: u32 = starting.rsplit_once(" pid=").expect("pid=").1.parse().unwrap();
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
    let unknown_turn_started = log
        .lines()
        .any(|line| line.find("unknown notification").is_some_and(|at| line[at..].contains("turn/started")));
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

// ----- helpers ---------------------------------------------------------------------------

fn requests<'a>(state: &'a Value, method: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    state["requests"].as_array().into_iter().flatten().filter(move |request| request["method"] == method)
}

fn goal_set_statuses(state: &Value) -> Vec<String> {
    requests(state, "thread/goal/set").map(|request| request["params"]["status"].as_str().unwrap().to_string()).collect()
}

fn last_turn_start(state: &Value) -> &Value {
    state["lastTurnStarts"].as_array().and_then(|turns| turns.last()).expect("a turn start")
}

fn array_len(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn days(n: u64) -> Duration {
    secs(n * 86_400)
}
