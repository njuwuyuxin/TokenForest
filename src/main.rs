use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Frame, Terminal};

mod config;
mod net_monitor;

use config::{AppConfig, SmoothingConfig};
use net_monitor::{CodexNetMetrics, CodexNetMonitor, PidThroughput, TrackedTool};

fn main() -> Result<()> {
    let config = AppConfig::load_default()?;
    let mut terminal = setup_terminal()?;
    let run_result = run_app(&mut terminal, config);
    restore_terminal(&mut terminal)?;
    run_result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, config: AppConfig) -> Result<()> {
    let tick_rate = Duration::from_millis(80);
    let mut app = App::new(config);

    while app.running {
        terminal.draw(|frame| render(frame, &app))?;

        let timeout = tick_rate.saturating_sub(app.last_tick.elapsed());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key.code);
        }

        if app.last_tick.elapsed() >= tick_rate {
            app.tick();
        }
    }

    Ok(())
}

struct App {
    running: bool,
    show_status: bool,
    last_tick: Instant,
    last_network_poll: Instant,
    frame_index: u64,
    token_rate: f32,
    target_token_rate: f32,
    smoothed_rate: f32,
    rain_intensity: f32,
    net_metrics: CodexNetMetrics,
    monitor: CodexNetMonitor,
    token_rate_history: VecDeque<f32>,
    config: AppConfig,
}

impl App {
    fn new(config: AppConfig) -> Self {
        let now = Instant::now();
        let poll_interval = config.network.poll_interval();
        Self {
            running: true,
            show_status: true,
            last_tick: now,
            last_network_poll: now.checked_sub(poll_interval).unwrap_or(now),
            frame_index: 0,
            token_rate: 0.0,
            target_token_rate: 0.0,
            smoothed_rate: 0.0,
            rain_intensity: 0.0,
            net_metrics: CodexNetMetrics {
                pid_count: 0,
                codex_pid_count: 0,
                claude_pid_count: 0,
                connection_count: 0,
                rx_bytes_per_sec: 0.0,
                tx_bytes_per_sec: 0.0,
                per_pid: Vec::new(),
                sample_error: None,
            },
            monitor: CodexNetMonitor::new(),
            token_rate_history: VecDeque::with_capacity(config.smoothing.window_size),
            config,
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.running = false,
            KeyCode::Char('s') => self.show_status = !self.show_status,
            _ => {}
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().max(1e-3);
        self.last_tick = now;
        self.frame_index = self.frame_index.saturating_add(1);

        if self.last_network_poll.elapsed() >= self.config.network.poll_interval() {
            self.last_network_poll = now;
            self.net_metrics = self.monitor.sample();
            let total_bytes_per_sec =
                (self.net_metrics.rx_bytes_per_sec + self.net_metrics.tx_bytes_per_sec) as f32;
            self.token_rate = total_bytes_per_sec / self.config.network.bytes_per_token_estimate;
            self.token_rate_history.push_back(self.token_rate.max(0.0));
            while self.token_rate_history.len() > self.config.smoothing.window_size {
                self.token_rate_history.pop_front();
            }
            self.target_token_rate =
                robust_target_rate(&self.token_rate_history, &self.config.smoothing);
        }

        let tau = if self.target_token_rate > self.smoothed_rate {
            self.config.smoothing.tau_rise_seconds
        } else {
            self.config.smoothing.tau_fall_seconds
        };
        let alpha = 1.0 - (-dt / tau).exp();
        self.smoothed_rate += (self.target_token_rate - self.smoothed_rate) * alpha;
        self.rain_intensity =
            (self.smoothed_rate / self.config.render.max_token_rate).clamp(0.0, 1.0);
    }
}

fn render(frame: &mut Frame, app: &App) {
    if app.show_status {
        let status_content_width = frame.area().width.saturating_sub(4) as usize;
        let line1 = format!(
            "Cyber Bonsai | q/esc quit | s toggle status | token est {:>7.1}/s | rain {:>5.1}%",
            app.smoothed_rate,
            app.rain_intensity * 100.0
        );
        let line2 = format!(
            "pids {:>2} (codex {:>2} claude {:>2}) | tcp {:>2} | rx {}/s | tx {}/s",
            app.net_metrics.pid_count,
            app.net_metrics.codex_pid_count,
            app.net_metrics.claude_pid_count,
            app.net_metrics.connection_count,
            format_rate(app.net_metrics.rx_bytes_per_sec),
            format_rate(app.net_metrics.tx_bytes_per_sec),
        );
        let line3 = format_pid_breakdown(&app.net_metrics.per_pid, status_content_width);

        let mut lines = vec![
            truncate_chars(&line1, status_content_width),
            truncate_chars(&line2, status_content_width),
            truncate_chars(&line3, status_content_width),
        ];
        if let Some(err) = &app.net_metrics.sample_error {
            lines.push(truncate_chars(err, status_content_width));
        }
        let status_height = lines.len() as u16 + 2;
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(status_height), Constraint::Min(1)])
            .split(frame.area());
        let status = Paragraph::new(lines.join("\n"))
            .block(Block::default().borders(Borders::ALL).title("Status"));
        frame.render_widget(status, areas[0]);
        frame.render_widget(RainForestWidget { app }, areas[1]);
    } else {
        frame.render_widget(RainForestWidget { app }, frame.area());
    }
}

struct RainForestWidget<'a> {
    app: &'a App,
}

impl Widget for RainForestWidget<'_> {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }

        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                put(buf, x, y, ' ', Style::default().bg(Color::Black));
            }
        }

        let ground_y = area.y + area.height.saturating_sub(3);
        for x in area.x..area.x + area.width {
            put(
                buf,
                x,
                ground_y,
                '_',
                Style::default()
                    .fg(Color::Rgb(14, 110, 36))
                    .bg(Color::Black),
            );
            put(
                buf,
                x,
                ground_y + 1,
                '.',
                Style::default().fg(Color::Green).bg(Color::Black),
            );
        }

        let tree_count = (area.width / 14).max(3);
        let step = area.width / tree_count.max(1);
        for i in 0..tree_count {
            let base_x = area.x + i * step + step / 2;
            let trunk_x = base_x.clamp(area.x + 1, area.x + area.width - 2);
            let trunk_h = 3 + (mix(i as u64) % 4) as u16;
            for j in 0..trunk_h {
                let y = ground_y.saturating_sub(j + 1);
                if y > area.y {
                    put(
                        buf,
                        trunk_x,
                        y,
                        '|',
                        Style::default()
                            .fg(Color::Rgb(120, 82, 52))
                            .bg(Color::Black),
                    );
                }
            }

            let crown_y = ground_y.saturating_sub(trunk_h + 1);
            for dx in -2i16..=2 {
                let x = trunk_x.saturating_add_signed(dx);
                if x > area.x && x < area.x + area.width - 1 && crown_y > area.y {
                    put(
                        buf,
                        x,
                        crown_y,
                        '^',
                        Style::default().fg(Color::Green).bg(Color::Black),
                    );
                }
            }
            if crown_y > area.y + 1 {
                put(
                    buf,
                    trunk_x,
                    crown_y - 1,
                    '^',
                    Style::default().fg(Color::LightGreen).bg(Color::Black),
                );
            }
        }

        let rain_chance = 0.03 + 0.42 * self.app.rain_intensity;
        let rain_phase = self.app.frame_index as u32;
        for y in area.y..ground_y {
            for x in area.x..area.x + area.width {
                if buf[(x, y)].symbol() != " " {
                    continue;
                }
                let pseudo_y = y.wrapping_add((rain_phase % 31) as u16);
                let sample = noise3(x as u32, pseudo_y as u32, rain_phase);
                let threshold = (rain_chance * 1000.0) as u32;
                if sample % 1000 < threshold {
                    let ch = if (x + y + rain_phase as u16).is_multiple_of(4) {
                        '|'
                    } else {
                        '\''
                    };
                    let color = if self.app.rain_intensity > 0.7 {
                        Color::Cyan
                    } else {
                        Color::Blue
                    };
                    put(buf, x, y, ch, Style::default().fg(color).bg(Color::Black));
                }
            }
        }

        if self.app.rain_intensity > 0.86 && self.app.frame_index % 23 < 2 {
            let bolt_x = area.x
                + 1
                + (noise3(17, 29, rain_phase) % (area.width.saturating_sub(2) as u32)) as u16;
            for y in area.y + 1..ground_y {
                put(
                    buf,
                    bolt_x,
                    y,
                    '|',
                    Style::default().fg(Color::White).bg(Color::Black),
                );
            }
        }

        let title = " cyber bonsai prototype ";
        for (i, ch) in title.chars().enumerate() {
            let x = area.x + 1 + i as u16;
            if x < area.x + area.width - 1 && area.y + 1 < ground_y {
                put(
                    buf,
                    x,
                    area.y + 1,
                    ch,
                    Style::default().fg(Color::Gray).bg(Color::Black),
                );
            }
        }
    }
}

fn put(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, ch: char, style: Style) {
    buf[(x, y)].set_char(ch).set_style(style);
}

fn noise3(a: u32, b: u32, c: u32) -> u32 {
    let n = ((a as u64) << 32) ^ ((b as u64) << 11) ^ (c as u64);
    (mix(n) & 0xffff_ffff) as u32
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^ (x >> 33)
}

fn format_rate(bytes_per_sec: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes_per_sec.max(0.0);
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < units.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{value:.0}{}", units[unit_index])
    } else {
        format!("{value:.1}{}", units[unit_index])
    }
}

fn robust_target_rate(history: &VecDeque<f32>, smoothing: &SmoothingConfig) -> f32 {
    if history.is_empty() {
        return 0.0;
    }
    let mut sorted = history.iter().copied().collect::<Vec<_>>();
    sorted.sort_by(|left, right| left.total_cmp(right));

    let len = sorted.len();
    let median = sorted[len / 2];
    let percentile_index =
        (((len.saturating_sub(1)) as f32) * smoothing.clip_percentile).round() as usize;
    let clip_base = sorted[percentile_index.min(len - 1)];
    let latest = history.back().copied().unwrap_or(0.0);
    let clipped_latest = latest.min(clip_base * smoothing.clip_multiplier + smoothing.clip_offset);
    (median * smoothing.median_weight + clipped_latest * smoothing.latest_weight).max(0.0)
}

fn format_pid_breakdown(per_pid: &[PidThroughput], max_chars: usize) -> String {
    if per_pid.is_empty() {
        return String::from("pid throughput: -");
    }
    let mut line = String::from("pid throughput: ");
    for entry in per_pid {
        let chunk = format!(
            "{}:{} rx {}/s tx {}/s c{}",
            tool_tag(entry.tool),
            entry.pid,
            format_rate(entry.rx_bytes_per_sec),
            format_rate(entry.tx_bytes_per_sec),
            entry.connection_count
        );
        if line.len() + chunk.len() + 3 > max_chars.saturating_sub(3) {
            line.push_str("...");
            break;
        }
        if line != "pid throughput: " {
            line.push_str(" | ");
        }
        line.push_str(&chunk);
    }
    line
}

fn tool_tag(tool: TrackedTool) -> &'static str {
    match tool {
        TrackedTool::Codex => "codex",
        TrackedTool::ClaudeCode => "claude",
    }
}

fn truncate_chars(error: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in error.chars().take(max_chars) {
        out.push(ch);
    }
    out
}
