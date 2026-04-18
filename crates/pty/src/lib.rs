use std::io::{Read as _, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
pub use portable_pty::CommandBuilder;
use portable_pty::{Child as PtyChild, PtySize, native_pty_system};

const DEFAULT_ROWS: u16 = 30;
const DEFAULT_COLS: u16 = 120;

pub struct PtyOptions {
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
        }
    }
}

pub struct PtyTransport {
    child: Box<dyn PtyChild + Send>,
    writer: Option<Box<dyn Write + Send>>,
    reader: Option<PtyReaderThread>,
}

struct PtySharedState {
    transcript: Vec<u8>,
    parser: vt100::Parser,
}

struct PtyReaderThread {
    state: Arc<Mutex<PtySharedState>>,
    handle: JoinHandle<()>,
}

pub enum PtyKey {
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Tab,
    Backspace,
    CtrlC,
}

pub struct PtySnapshot {
    pub screen: String,
    pub transcript: String,
    pub transcript_debug: String,
}

impl PtyTransport {
    pub fn spawn(command: CommandBuilder, options: PtyOptions) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: options.rows,
                cols: options.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;
        let child = pair
            .slave
            .spawn_command(command)
            .context("failed to spawn PTY child")?;
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;
        let state = Arc::new(Mutex::new(PtySharedState {
            transcript: Vec::new(),
            parser: vt100::Parser::new(options.rows, options.cols, 0),
        }));
        let thread_state = Arc::clone(&state);
        let handle = std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0_u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut locked) = thread_state.lock() {
                            locked.transcript.extend_from_slice(&buf[..n]);
                            locked.parser.process(&buf[..n]);
                        } else {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            child,
            writer: Some(writer),
            reader: Some(PtyReaderThread { state, handle }),
        })
    }

    pub fn write_line(&mut self, text: &str) -> Result<()> {
        self.send_text(text)?;
        self.send_key(PtyKey::Enter)
    }

    pub fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_bytes(text.as_bytes())
    }

    pub fn send_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("PTY input unavailable"))?;
        writer
            .write_all(bytes)
            .context("failed to write PTY input")?;
        writer.flush().context("failed to flush PTY input")?;
        Ok(())
    }

    pub fn send_key(&mut self, key: PtyKey) -> Result<()> {
        let bytes: &[u8] = match key {
            PtyKey::Enter => b"\r",
            PtyKey::Esc => b"\x1b",
            PtyKey::Up => b"\x1b[A",
            PtyKey::Down => b"\x1b[B",
            PtyKey::Left => b"\x1b[D",
            PtyKey::Right => b"\x1b[C",
            PtyKey::Tab => b"\t",
            PtyKey::Backspace => b"\x7f",
            PtyKey::CtrlC => b"\x03",
        };
        self.send_bytes(bytes)
    }

    pub fn snapshot(&self) -> Result<PtySnapshot> {
        snapshot_from_reader(self.reader.as_ref())
    }

    pub async fn wait_for_screen_contains(
        &self,
        needle: &str,
        timeout: Duration,
    ) -> Result<PtySnapshot> {
        self.wait_for_snapshot(
            timeout,
            |snapshot| snapshot.screen.contains(needle),
            |snapshot| {
                format!(
                    "PTY screen did not contain {:?}\n{}",
                    needle,
                    render_snapshot(snapshot)
                )
            },
        )
        .await
    }

    pub async fn wait_for_contains(&self, needle: &str, timeout: Duration) -> Result<PtySnapshot> {
        self.wait_for_snapshot(
            timeout,
            |snapshot| snapshot.screen.contains(needle) || snapshot.transcript.contains(needle),
            |snapshot| {
                format!(
                    "PTY did not contain {:?} in screen or transcript\n{}",
                    needle,
                    render_snapshot(snapshot)
                )
            },
        )
        .await
    }

    pub async fn wait_for_transcript_contains(
        &self,
        needle: &str,
        timeout: Duration,
    ) -> Result<PtySnapshot> {
        self.wait_for_snapshot(
            timeout,
            |snapshot| snapshot.transcript.contains(needle),
            |snapshot| {
                format!(
                    "PTY transcript did not contain {:?}\n{}",
                    needle,
                    render_snapshot(snapshot)
                )
            },
        )
        .await
    }

    pub async fn wait_for_nonempty_screen(&self, timeout: Duration) -> Result<PtySnapshot> {
        self.wait_for_snapshot(
            timeout,
            |snapshot| snapshot.screen.lines().any(|line| !line.trim().is_empty()),
            |snapshot| format!("PTY screen stayed empty\n{}", render_snapshot(snapshot)),
        )
        .await
    }

    pub async fn wait_for_snapshot<F, E>(
        &self,
        timeout: Duration,
        predicate: F,
        error: E,
    ) -> Result<PtySnapshot>
    where
        F: Fn(&PtySnapshot) -> bool,
        E: Fn(&PtySnapshot) -> String,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot()?;
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(error(&snapshot));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn shutdown(&mut self) {
        let _ = self.writer.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.handle.join();
        }
    }
}

fn snapshot_from_reader(reader: Option<&PtyReaderThread>) -> Result<PtySnapshot> {
    let Some(reader) = reader else {
        return Ok(PtySnapshot {
            screen: String::new(),
            transcript: String::new(),
            transcript_debug: String::new(),
        });
    };
    let locked = reader
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to lock PTY state"))?;
    let screen = normalize_screen(&locked.parser.screen().contents());
    let transcript = String::from_utf8_lossy(&locked.transcript).to_string();
    let transcript_debug = debug_bytes(&locked.transcript);
    Ok(PtySnapshot {
        screen,
        transcript,
        transcript_debug,
    })
}

fn normalize_screen(screen: &str) -> String {
    screen
        .replace('\0', "")
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tail(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars().collect::<Vec<_>>();
    if chars.len() > max_chars {
        chars.drain(..chars.len() - max_chars);
    }
    chars.into_iter().collect()
}

fn render_snapshot(snapshot: &PtySnapshot) -> String {
    format!(
        "screen:\n{}\n\ntranscript:\n{}\n\ntranscript_debug:\n{}",
        snapshot.screen,
        tail(&snapshot.transcript, 4000),
        tail(&snapshot.transcript_debug, 4000)
    )
}

fn debug_bytes(bytes: &[u8]) -> String {
    let mut rendered = String::new();
    for &byte in bytes {
        match byte {
            b'\n' => rendered.push_str("\\n\n"),
            b'\r' => rendered.push_str("\\r"),
            b'\t' => rendered.push_str("\\t"),
            0x20..=0x7e => rendered.push(byte as char),
            _ => rendered.push_str(&format!("\\x{byte:02x}")),
        }
    }
    rendered
}
