use std::time::Duration;

use anyhow::Result;
use portable_pty::CommandBuilder;
use test_orch::interact::pty::{PtyKey, PtyOptions, PtySnapshot, PtyTransport};

#[tokio::test(flavor = "multi_thread")]
async fn pty_wait_helpers_and_basic_snapshot_work() -> Result<()> {
    let mut pty = spawn_fixture()?;

    let boot = pty
        .wait_for_nonempty_screen(Duration::from_secs(10))
        .await?;
    assert_screen_contains(&boot, "READY");

    let ready = pty
        .wait_for_screen_contains("CURSOR:0", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&ready, "BUFFER:");
    assert_screen_contains(&ready, "LINES:");

    pty.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_send_text_backspace_and_write_line_match_cli_semantics() -> Result<()> {
    let mut pty = spawn_fixture()?;
    pty.wait_for_screen_contains("READY", Duration::from_secs(10))
        .await?;

    pty.send_text("abc")?;
    let typed = pty
        .wait_for_contains("BUFFER:abc", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&typed, "BUFFER:abc");
    assert!(
        typed.transcript.contains("abc"),
        "{}",
        render_snapshot(&typed)
    );

    pty.send_key(PtyKey::Backspace)?;
    let erased = pty
        .wait_for_contains("BUFFER:ab", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&erased, "BUFFER:ab");

    pty.write_line("!")?;
    let submitted = pty
        .wait_for_contains("LINES:ab!", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&submitted, "BUFFER:");
    assert_screen_contains(&submitted, "LINES:ab!");
    assert!(
        submitted.transcript.contains("LINE:ab!"),
        "{}",
        render_snapshot(&submitted)
    );

    pty.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_arrow_navigation_and_editing_keys_reach_child_correctly() -> Result<()> {
    let mut pty = spawn_fixture()?;
    pty.wait_for_screen_contains("READY", Duration::from_secs(10))
        .await?;

    pty.send_text("ac")?;
    pty.wait_for_contains("BUFFER:ac", Duration::from_secs(5))
        .await?;

    pty.send_key(PtyKey::Left)?;
    let left = pty
        .wait_for_contains("LAST_KEY:LEFT", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&left, "CURSOR:1");
    assert!(
        left.transcript.contains("KEY:LEFT"),
        "{}",
        render_snapshot(&left)
    );

    pty.send_text("b")?;
    let inserted = pty
        .wait_for_contains("BUFFER:abc", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&inserted, "CURSOR:2");

    pty.send_key(PtyKey::Right)?;
    let right = pty
        .wait_for_contains("CURSOR:3", Duration::from_secs(5))
        .await?;
    assert!(
        right.transcript.contains("KEY:RIGHT"),
        "{}",
        render_snapshot(&right)
    );

    pty.send_key(PtyKey::Tab)?;
    let tabbed = pty
        .wait_for_contains("BUFFER:abc<TAB>", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&tabbed, "BUFFER:abc<TAB>");

    pty.send_key(PtyKey::Esc)?;
    let escaped = pty
        .wait_for_contains("ESC_COUNT:1", Duration::from_secs(5))
        .await?;
    assert!(
        escaped.transcript_debug.contains("\\x1b"),
        "{}",
        render_snapshot(&escaped)
    );
    assert!(
        escaped.transcript.contains("KEY:ESC"),
        "{}",
        render_snapshot(&escaped)
    );

    pty.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_special_keys_up_down_and_ctrl_c_are_observable() -> Result<()> {
    let mut pty = spawn_fixture()?;
    pty.wait_for_screen_contains("READY", Duration::from_secs(10))
        .await?;

    pty.send_key(PtyKey::Up)?;
    let up = pty
        .wait_for_contains("LAST_KEY:UP", Duration::from_secs(5))
        .await?;
    assert!(up.transcript.contains("KEY:UP"), "{}", render_snapshot(&up));

    pty.send_key(PtyKey::Down)?;
    let down = pty
        .wait_for_contains("LAST_KEY:DOWN", Duration::from_secs(5))
        .await?;
    assert!(
        down.transcript.contains("KEY:DOWN"),
        "{}",
        render_snapshot(&down)
    );

    pty.send_key(PtyKey::CtrlC)?;
    let ctrl_c = pty
        .wait_for_contains("LAST_KEY:CTRL_C", Duration::from_secs(5))
        .await?;
    assert!(
        ctrl_c.transcript.contains("KEY:CTRL_C"),
        "{}",
        render_snapshot(&ctrl_c)
    );

    pty.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_screen_reflects_latest_redraw_while_transcript_retains_history() -> Result<()> {
    let mut pty = spawn_fixture()?;
    pty.wait_for_screen_contains("READY", Duration::from_secs(10))
        .await?;

    pty.write_line("first")?;
    let first = pty
        .wait_for_contains("LINES:first", Duration::from_secs(5))
        .await?;
    assert!(
        first.transcript.contains("LINE:first"),
        "{}",
        render_snapshot(&first)
    );

    pty.write_line("second")?;
    let second = pty
        .wait_for_contains("LINES:first|second", Duration::from_secs(5))
        .await?;
    assert_screen_contains(&second, "LINES:first|second");
    assert!(
        !second.screen.contains("LINE:first"),
        "screen should reflect latest visible state only\n{}",
        render_snapshot(&second)
    );
    assert!(
        second.transcript.contains("LINE:first"),
        "{}",
        render_snapshot(&second)
    );
    assert!(
        second.transcript.contains("LINE:second"),
        "{}",
        render_snapshot(&second)
    );

    pty.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_shutdown_is_safe_after_interaction() -> Result<()> {
    let mut pty = spawn_fixture()?;
    pty.wait_for_screen_contains("READY", Duration::from_secs(10))
        .await?;
    pty.send_text("shutdown-check")?;
    pty.wait_for_contains("BUFFER:shutdown-check", Duration::from_secs(5))
        .await?;

    pty.shutdown().await;

    let after = pty.snapshot()?;
    assert!(after.screen.is_empty(), "{}", render_snapshot(&after));
    assert!(after.transcript.is_empty(), "{}", render_snapshot(&after));
    Ok(())
}

fn spawn_fixture() -> Result<PtyTransport> {
    let mut command = CommandBuilder::new("python3");
    command.arg("-u");
    command.arg("-c");
    command.arg(FIXTURE_SCRIPT);
    PtyTransport::spawn(command, PtyOptions::default())
}

fn assert_screen_contains(snapshot: &PtySnapshot, needle: &str) {
    assert!(
        snapshot.screen.contains(needle),
        "expected screen to contain {needle:?}\n{}",
        render_snapshot(snapshot)
    );
}

fn render_snapshot(snapshot: &PtySnapshot) -> String {
    format!(
        "screen:\n{}\n\ntranscript:\n{}\n\ntranscript_debug:\n{}",
        snapshot.screen, snapshot.transcript, snapshot.transcript_debug
    )
}

const FIXTURE_SCRIPT: &str = r#"
import os
import select
import sys
import termios
import time
import tty

fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)

read_byte = lambda: os.read(fd, 1)

buffer = []
cursor = 0
lines = []
last_key = '-'
esc_count = 0
escape_state = None

def render():
    sys.stdout.write('\x1b[2J\x1b[H')
    sys.stdout.write('READY\n')
    sys.stdout.write('BUFFER:' + ''.join(buffer) + '\n')
    sys.stdout.write('CURSOR:' + str(cursor) + '\n')
    sys.stdout.write('LINES:' + '|'.join(lines) + '\n')
    sys.stdout.write('LAST_KEY:' + last_key + '\n')
    sys.stdout.write('ESC_COUNT:' + str(esc_count) + '\n')
    sys.stdout.flush()

def finish_escape(seq):
    global cursor, last_key, esc_count
    if seq == b'[A':
        last_key = 'UP'
        print('KEY:UP')
    elif seq == b'[B':
        last_key = 'DOWN'
        print('KEY:DOWN')
    elif seq == b'[C':
        if cursor < len(buffer):
            cursor += 1
        last_key = 'RIGHT'
        print('KEY:RIGHT')
    elif seq == b'[D':
        if cursor > 0:
            cursor -= 1
        last_key = 'LEFT'
        print('KEY:LEFT')
    else:
        esc_count += 1
        last_key = 'ESC'
        print('KEY:ESC')
    render()

try:
    render()
    while True:
        if escape_state == b'':
            seq = bytearray()
            deadline = time.monotonic() + 0.5
            while len(seq) < 2:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                ready, _, _ = select.select([fd], [], [], remaining)
                if not ready:
                    break
                nxt = read_byte()
                if not nxt:
                    break
                seq.extend(nxt)
            if len(seq) == 2:
                finish_escape(bytes(seq))
                escape_state = None
                continue
            esc_count += 1
            last_key = 'ESC'
            print('KEY:ESC')
            render()
            escape_state = None
            for leftover in seq:
                text = bytes([leftover]).decode('utf-8', errors='replace')
                buffer[cursor:cursor] = [text]
                cursor += 1
                render()
            continue

        ch = read_byte()
        if not ch:
            break

        if escape_state is not None:
            escape_state += ch
            if len(escape_state) == 2:
                finish_escape(escape_state)
                escape_state = None
            continue

        if ch == b'\x1b':
            escape_state = b''
            continue
        if ch == b'\x03':
            last_key = 'CTRL_C'
            print('KEY:CTRL_C')
            render()
            continue
        if ch == b'\r':
            line = ''.join(buffer)
            lines.append(line)
            buffer = []
            cursor = 0
            print('LINE:' + line)
            render()
            continue
        if ch == b'\x7f':
            if cursor > 0:
                cursor -= 1
                buffer.pop(cursor)
            render()
            continue
        if ch == b'\t':
            buffer[cursor:cursor] = list('<TAB>')
            cursor += len('<TAB>')
            last_key = 'TAB'
            print('KEY:TAB')
            render()
            continue
        text = ch.decode('utf-8', errors='replace')
        buffer[cursor:cursor] = [text]
        cursor += 1
        render()
finally:
    if escape_state == b'':
        esc_count += 1
        last_key = 'ESC'
        print('KEY:ESC')
        render()
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
"#;
