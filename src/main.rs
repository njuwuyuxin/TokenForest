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

const LIGHTNING_ACCEL_THRESHOLD: f32 = 260.0;
const LIGHTNING_COOLDOWN: Duration = Duration::from_secs(10);
const LIGHTNING_FLASH_DURATION: Duration = Duration::from_millis(140);

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
    prev_smoothed_rate: f32,
    token_accel: f32,
    last_lightning_trigger: Option<Instant>,
    lightning_flash_until: Option<Instant>,
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
            prev_smoothed_rate: 0.0,
            token_accel: 0.0,
            last_lightning_trigger: None,
            lightning_flash_until: None,
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
        self.token_accel = (self.smoothed_rate - self.prev_smoothed_rate) / dt;
        self.prev_smoothed_rate = self.smoothed_rate;

        if self.token_accel >= LIGHTNING_ACCEL_THRESHOLD
            && self
                .last_lightning_trigger
                .is_none_or(|last| now.duration_since(last) >= LIGHTNING_COOLDOWN)
        {
            self.last_lightning_trigger = Some(now);
            self.lightning_flash_until = Some(now + LIGHTNING_FLASH_DURATION);
        }

        if self.lightning_flash_until.is_some_and(|until| now >= until) {
            self.lightning_flash_until = None;
        }
    }

    fn flash_active(&self) -> bool {
        self.lightning_flash_until
            .is_some_and(|until| Instant::now() < until)
    }
}

fn render(frame: &mut Frame, app: &App) {
    if app.show_status {
        let status_content_width = frame.area().width.saturating_sub(4) as usize;
        let line1 = format!(
            "TokenForest | q/esc quit | s toggle status | token est {:>7.1}/s | rain {:>5.1}%",
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

    if app.flash_active() {
        let flash = Block::default().style(Style::default().bg(Color::White));
        frame.render_widget(flash, frame.area());
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
        let mut horizon_y = area.y + area.height.saturating_mul(34) / 100;
        let max_horizon = ground_y.saturating_sub(5).max(area.y);
        if horizon_y > max_horizon {
            horizon_y = max_horizon;
        }
        let wind = wind_state(self.app.frame_index, self.app.rain_intensity);

        let mountain_style_far = Style::default()
            .fg(Color::Rgb(96, 104, 112))
            .bg(Color::Black);
        let mountain_count = ((area.width / 45).max(2)) as usize;
        for i in 0..mountain_count {
            let seed = mix(0x4455_8821 ^ ((i as u64) << 12) ^ ((area.width as u64) << 3));
            let span = (area.width as i32 / (4 + (seed % 2) as i32)).max(10);
            let x_base =
                area.x as i32 + ((i + 1) as i32 * area.width as i32 / (mountain_count as i32 + 1));
            let peak_x = x_base + ((seed >> 9) % 9) as i32 - 4;
            let peak_y = horizon_y as i32 - 1 - ((seed >> 17) % 2) as i32;
            draw_mountain_outline(
                buf,
                area,
                MountainSpec {
                    peak_x,
                    peak_y,
                    half_width: span,
                    height: 4 + ((seed >> 21) % 2) as i32,
                    seed,
                    style: mountain_style_far,
                },
            );
        }

        let ridge_style_far = Style::default().fg(Color::Rgb(78, 88, 94)).bg(Color::Black);
        for x in area.x..area.x + area.width {
            let ridge_far = horizon_y.saturating_add((noise3(x as u32, 91, 7) % 3) as u16);
            if buf[(x, ridge_far)].symbol() == " " && (x - area.x).is_multiple_of(3) {
                put(buf, x, ridge_far, '~', ridge_style_far);
            }
        }

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
        draw_ground_details(buf, area, ground_y);

        let tree_count = ((area.width / 8).max(8)) as usize;
        let mut trees = Vec::with_capacity(tree_count);
        for i in 0..tree_count {
            let seed = mix((i as u64) ^ ((area.width as u64) << 17) ^ ((area.height as u64) << 5));
            let depth_raw = (seed & 0xffff) as f32 / 65535.0;
            let depth = depth_raw.powf(1.55);
            let x_span = area.width.saturating_sub(2).max(1) as u32;
            let x = area.x as i32 + 1 + (noise3(i as u32, 23, 77) % x_span) as i32;
            let y = lerp_f32(horizon_y as f32 + 2.0, ground_y as f32 - 1.0, depth).round() as i32;
            trees.push(TreeInstance {
                center_x: x,
                base_y: y,
                depth,
                seed,
            });
        }
        trees.sort_by(|left, right| left.depth.total_cmp(&right.depth));
        for tree in trees {
            draw_tree(buf, area, tree);
        }

        let rain_bands = rain_bands(self.app.rain_intensity);
        let rain_phase = self.app.frame_index as u32;
        for y in area.y..ground_y {
            for x in area.x..area.x + area.width {
                if buf[(x, y)].symbol() != " " {
                    continue;
                }

                let vertical_depth = ((y.saturating_sub(horizon_y)) as f32
                    / (ground_y.saturating_sub(horizon_y)).max(1) as f32)
                    .clamp(0.0, 1.0);
                let x_i = x as i32;
                let y_i = y as i32;
                let side_shift = (rain_phase as f32 * wind.side_speed).round() as i32;
                let fall_shift = (rain_phase as f32 * wind.fall_speed).round() as i32;
                let line_x = x_i + side_shift - ((y_i * wind.tilt_milli) / 1024);
                let perturb = if noise3(x as u32 + 23, y as u32 + 17, rain_phase ^ 0x00ab_cdef)
                    .is_multiple_of(19)
                {
                    (noise3(x as u32 + 11, y as u32 + 73, rain_phase) % 3) as i32 - 1
                } else {
                    0
                };
                let motion_x = (line_x + perturb + 65_536) as u32;
                let motion_y = (y_i + fall_shift + 65_536) as u32;
                let sample = (noise3(motion_x, motion_y, rain_phase ^ 0x9e37_79b9) % 10_000) as f32
                    / 10_000.0;
                let direction_variant =
                    noise3(x as u32 + 41, y as u32 + 29, rain_phase ^ 0x00de_ad11);

                let depth_gain = 0.18 + 1.05 * vertical_depth;
                let mut threshold = rain_bands.heavy * depth_gain;
                if sample < threshold {
                    let heavy_char = rain_direction_char(wind.tilt, direction_variant);
                    put(
                        buf,
                        x,
                        y,
                        heavy_char,
                        Style::default().fg(Color::LightCyan).bg(Color::Black),
                    );
                    let next_y = y.saturating_add(1);
                    if next_y < ground_y
                        && buf[(x, next_y)].symbol() == " "
                        && noise3(x as u32 + 5, y as u32, rain_phase).is_multiple_of(3)
                    {
                        let tail_char =
                            rain_direction_char(wind.tilt * 0.75, direction_variant + 3);
                        put(
                            buf,
                            x,
                            next_y,
                            tail_char,
                            Style::default().fg(Color::Cyan).bg(Color::Black),
                        );
                    }
                    continue;
                }

                threshold += rain_bands.streak * depth_gain;
                if sample < threshold {
                    let streak = rain_direction_char(wind.tilt, direction_variant + 11);
                    put(
                        buf,
                        x,
                        y,
                        streak,
                        Style::default().fg(Color::Cyan).bg(Color::Black),
                    );
                    continue;
                }

                threshold += rain_bands.drizzle * (0.35 + 0.75 * vertical_depth);
                if sample < threshold {
                    let drop = if wind.tilt.abs() < 0.24 {
                        if noise3(x as u32 + 37, y as u32, rain_phase).is_multiple_of(2) {
                            '\''
                        } else {
                            '`'
                        }
                    } else if wind.tilt > 0.0 {
                        if direction_variant.is_multiple_of(7) {
                            '|'
                        } else {
                            '/'
                        }
                    } else {
                        if direction_variant.is_multiple_of(7) {
                            '|'
                        } else {
                            '\\'
                        }
                    };
                    put(
                        buf,
                        x,
                        y,
                        drop,
                        Style::default().fg(Color::Blue).bg(Color::Black),
                    );
                    continue;
                }

                threshold += rain_bands.mist * (0.65 + 0.45 * (1.0 - vertical_depth));
                if sample < threshold {
                    put(
                        buf,
                        x,
                        y,
                        '.',
                        Style::default()
                            .fg(Color::Rgb(95, 124, 168))
                            .bg(Color::Black),
                    );
                }
            }
        }

        if self.app.rain_intensity > 0.9 && self.app.frame_index % 29 < 2 {
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

        draw_token_forest_logo(buf, area, ground_y, self.app.frame_index);
    }
}

fn put(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, ch: char, style: Style) {
    buf[(x, y)].set_char(ch).set_style(style);
}

#[derive(Clone, Copy)]
struct RainBands {
    mist: f32,
    drizzle: f32,
    streak: f32,
    heavy: f32,
}

#[derive(Clone, Copy)]
struct WindField {
    tilt: f32,
    tilt_milli: i32,
    side_speed: f32,
    fall_speed: f32,
}

fn rain_bands(intensity: f32) -> RainBands {
    if intensity < 0.08 {
        RainBands {
            mist: 0.0015,
            drizzle: 0.0007,
            streak: 0.0,
            heavy: 0.0,
        }
    } else if intensity < 0.2 {
        RainBands {
            mist: 0.003,
            drizzle: 0.0018,
            streak: 0.0006,
            heavy: 0.0,
        }
    } else if intensity < 0.38 {
        RainBands {
            mist: 0.005,
            drizzle: 0.0035,
            streak: 0.0016,
            heavy: 0.0003,
        }
    } else if intensity < 0.58 {
        RainBands {
            mist: 0.007,
            drizzle: 0.006,
            streak: 0.003,
            heavy: 0.001,
        }
    } else if intensity < 0.78 {
        RainBands {
            mist: 0.009,
            drizzle: 0.009,
            streak: 0.005,
            heavy: 0.002,
        }
    } else {
        RainBands {
            mist: 0.011,
            drizzle: 0.013,
            streak: 0.009,
            heavy: 0.004,
        }
    }
}

fn wind_state(frame_index: u64, rain_intensity: f32) -> WindField {
    let t = frame_index as f32 * 0.08;
    let base = (t * 0.33).sin() * 0.48;
    let slow = (t * 0.11 + 1.3).sin() * 0.32;
    let gust = (t * 0.9).sin().powi(3) * (0.22 + rain_intensity * 0.2);
    let tilt = (base + slow + gust).clamp(-1.15, 1.15);
    let side_speed = tilt * (0.45 + rain_intensity * 0.5);
    let fall_speed = 4.0 + rain_intensity * 3.2 + (tilt.abs() * 0.3);
    WindField {
        tilt,
        tilt_milli: (tilt * 512.0).round() as i32,
        side_speed,
        fall_speed,
    }
}

fn rain_direction_char(tilt: f32, variant: u32) -> char {
    if tilt > 0.42 {
        if variant.is_multiple_of(9) { '|' } else { '/' }
    } else if tilt < -0.42 {
        if variant.is_multiple_of(9) { '|' } else { '\\' }
    } else if tilt > 0.18 {
        if variant.is_multiple_of(5) { '|' } else { '/' }
    } else if tilt < -0.18 {
        if variant.is_multiple_of(5) { '|' } else { '\\' }
    } else {
        '|'
    }
}

#[derive(Clone, Copy)]
struct TreeInstance {
    center_x: i32,
    base_y: i32,
    depth: f32,
    seed: u64,
}

fn draw_tree(buf: &mut ratatui::buffer::Buffer, area: ratatui::layout::Rect, tree: TreeInstance) {
    let depth = tree.depth.clamp(0.0, 1.0);
    let trunk_h = lerp_f32(2.5, 8.0, depth).round() as i32 + ((tree.seed >> 7) % 2) as i32;
    let trunk_w = if depth > 0.72 { 2 } else { 1 };

    let center = tree.center_x;
    let ground = tree.base_y;
    let trunk_left = center - trunk_w / 2;
    let trunk_right = trunk_left + trunk_w - 1;
    let bark_color = rgb_lerp((82, 62, 44), (136, 96, 60), depth);
    let bark_style = Style::default().fg(bark_color).bg(Color::Black);

    for layer in 0..trunk_h {
        let y = ground - 1 - layer;
        for x in trunk_left..=trunk_right {
            let ch = if trunk_w == 2 && x == trunk_right && layer % 2 != 0 {
                ':'
            } else {
                '|'
            };
            put_if_inside(buf, area, x, y, ch, bark_style);
        }
    }

    if depth > 0.52 {
        let root_style = Style::default()
            .fg(rgb_lerp((74, 53, 36), (102, 70, 44), depth))
            .bg(Color::Black);
        put_if_inside(buf, area, trunk_left - 1, ground - 1, '/', root_style);
        put_if_inside(buf, area, trunk_right + 1, ground - 1, '\\', root_style);
    }

    let is_conifer = ((tree.seed >> 3) & 1) == 0;
    let crown_base_y = ground - trunk_h;
    if is_conifer {
        let tiers = if depth < 0.34 {
            2
        } else if depth < 0.66 {
            3
        } else {
            4
        };
        let top_y = crown_base_y - (tiers * 2);
        for tier in 0..tiers {
            let y = top_y + tier * 2;
            let span = 1 + tier * 2 + if depth > 0.72 { 1 } else { 0 };
            for row in 0..2 {
                let row_y = y + row;
                let row_span = span + row;
                for dx in -row_span..=row_span {
                    let x = center + dx;
                    let edge = dx.abs() == row_span;
                    let ch = if edge {
                        if dx < 0 { '/' } else { '\\' }
                    } else if noise3((x + 23) as u32, (row_y + 17) as u32, tree.seed as u32)
                        .is_multiple_of(7)
                    {
                        '*'
                    } else {
                        '^'
                    };
                    let row_t = (tier as f32 + row as f32 * 0.35) / tiers.max(1) as f32;
                    let leaf_color = rgb_lerp(
                        (42, 112, 54),
                        (102, 182, 108),
                        (depth * 0.7 + row_t * 0.3).clamp(0.0, 1.0),
                    );
                    put_if_inside(
                        buf,
                        area,
                        x,
                        row_y,
                        ch,
                        Style::default().fg(leaf_color).bg(Color::Black),
                    );
                }
            }
        }
        put_if_inside(
            buf,
            area,
            center,
            top_y - 1,
            '^',
            Style::default()
                .fg(rgb_lerp((68, 150, 74), (122, 205, 126), depth))
                .bg(Color::Black),
        );
    } else {
        let canopy_h = if depth < 0.38 {
            3
        } else if depth < 0.68 {
            4
        } else {
            5
        };
        let top_y = crown_base_y - canopy_h;
        let radius = lerp_f32(2.0, 5.2, depth);
        for row in 0..canopy_h {
            let y = top_y + row;
            let row_t = if canopy_h > 1 {
                row as f32 / (canopy_h - 1) as f32
            } else {
                0.0
            };
            let bell = 1.0 - ((row_t * 2.0 - 1.0).abs().powf(1.35));
            let span = (1.0 + bell * radius).round() as i32;
            for dx in -span..=span {
                let x = center + dx;
                let edge = dx.abs() == span;
                let ch = if edge {
                    if row == 0 {
                        if dx < 0 { '/' } else { '\\' }
                    } else if dx < 0 {
                        '('
                    } else {
                        ')'
                    }
                } else {
                    let n = noise3((x + 31) as u32, (y + 17) as u32, tree.seed as u32);
                    if n.is_multiple_of(13) {
                        'o'
                    } else if n.is_multiple_of(7) {
                        '*'
                    } else {
                        '^'
                    }
                };
                let leaf_color = rgb_lerp(
                    (46, 116, 58),
                    (114, 192, 118),
                    (depth * 0.78 + (1.0 - row_t) * 0.22).clamp(0.0, 1.0),
                );
                put_if_inside(
                    buf,
                    area,
                    x,
                    y,
                    ch,
                    Style::default().fg(leaf_color).bg(Color::Black),
                );
            }
        }
        put_if_inside(
            buf,
            area,
            center,
            top_y - 1,
            '^',
            Style::default()
                .fg(rgb_lerp((78, 165, 84), (134, 214, 138), depth))
                .bg(Color::Black),
        );
    }
}

#[derive(Clone, Copy)]
struct MountainSpec {
    peak_x: i32,
    peak_y: i32,
    half_width: i32,
    height: i32,
    seed: u64,
    style: Style,
}

fn draw_mountain_outline(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    spec: MountainSpec,
) {
    let peak_x = spec.peak_x;
    let peak_y = spec.peak_y;
    let half_width = spec.half_width;
    let height = spec.height;
    let seed = spec.seed;
    let style = spec.style;
    if half_width < 2 || height < 2 {
        return;
    }

    for dx in 0..=half_width {
        let base_y = peak_y + (dx * height) / half_width;
        let jitter = if (dx as u32).is_multiple_of(11) {
            (noise3(dx as u32, seed as u32, 171) % 3) as i32 - 1
        } else {
            0
        };
        let y = base_y + jitter;
        let left_x = peak_x - dx;
        let right_x = peak_x + dx;
        put_if_inside(buf, area, left_x, y, '/', style);
        put_if_inside(buf, area, right_x, y, '\\', style);
    }

    put_if_inside(buf, area, peak_x, peak_y, '^', style);
}

fn draw_ground_details(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    ground_y: u16,
) {
    let grass_style = Style::default()
        .fg(Color::Rgb(78, 172, 86))
        .bg(Color::Black);
    let stone_style = Style::default()
        .fg(Color::Rgb(142, 147, 154))
        .bg(Color::Black);
    let stone_shadow = Style::default()
        .fg(Color::Rgb(102, 108, 116))
        .bg(Color::Black);
    let soil_row = ground_y.saturating_add(1);
    if soil_row >= area.y + area.height {
        return;
    }

    let grass_count = ((area.width / 34).max(3)) as usize;
    for i in 0..grass_count {
        let spread_x =
            area.x as i32 + ((i as i32 + 1) * area.width as i32 / (grass_count as i32 + 1));
        let jitter = (noise3(i as u32, ground_y as u32, 0x0073_5a11) % 7) as i32 - 3;
        draw_grass_clump(
            buf,
            area,
            spread_x + jitter,
            ground_y as i32 - 1,
            grass_style,
        );
    }

    let rock_count = ((area.width / 50).max(2)) as usize;
    for i in 0..rock_count {
        let spread_x =
            area.x as i32 + ((i as i32 + 1) * area.width as i32 / (rock_count as i32 + 1));
        let jitter = (noise3(i as u32, ground_y as u32, 0x0091_4d2b) % 9) as i32 - 4;
        let rock_seed =
            mix((i as u64) ^ ((area.width as u64) << 7) ^ ((ground_y as u64) << 3) ^ 0x55aa_1177);
        draw_rock_cluster(
            buf,
            area,
            spread_x + jitter,
            soil_row as i32,
            rock_seed,
            stone_style,
            stone_shadow,
        );
    }
}

fn draw_grass_clump(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    center_x: i32,
    base_y: i32,
    style: Style,
) {
    let (left, center, right) = ('/', '|', '\\');

    put_if_inside(buf, area, center_x - 1, base_y, left, style);
    put_if_inside(buf, area, center_x, base_y - 1, center, style);
    put_if_inside(buf, area, center_x + 1, base_y, right, style);
    put_if_inside(buf, area, center_x, base_y, '|', style);
    put_if_inside(
        buf,
        area,
        center_x - 2,
        base_y + 1,
        ',',
        Style::default()
            .fg(Color::Rgb(64, 138, 72))
            .bg(Color::Black),
    );
    put_if_inside(
        buf,
        area,
        center_x + 2,
        base_y + 1,
        ',',
        Style::default()
            .fg(Color::Rgb(64, 138, 72))
            .bg(Color::Black),
    );
}

fn draw_rock_cluster(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    center_x: i32,
    base_y: i32,
    seed: u64,
    outline: Style,
    shade: Style,
) {
    match seed % 3 {
        0 => {
            put_if_inside(buf, area, center_x - 1, base_y - 1, '/', outline);
            put_if_inside(buf, area, center_x, base_y - 2, '_', outline);
            put_if_inside(buf, area, center_x + 1, base_y - 1, '\\', outline);
            put_if_inside(buf, area, center_x - 1, base_y, '\\', outline);
            put_if_inside(buf, area, center_x, base_y, '_', outline);
            put_if_inside(buf, area, center_x + 1, base_y, '/', outline);
            put_if_inside(buf, area, center_x, base_y - 1, '.', shade);
        }
        1 => {
            put_if_inside(buf, area, center_x - 2, base_y - 1, '/', outline);
            put_if_inside(buf, area, center_x - 1, base_y - 2, '_', outline);
            put_if_inside(buf, area, center_x, base_y - 2, '_', outline);
            put_if_inside(buf, area, center_x + 1, base_y - 1, '\\', outline);
            put_if_inside(buf, area, center_x - 2, base_y, '\\', outline);
            put_if_inside(buf, area, center_x - 1, base_y, '_', outline);
            put_if_inside(buf, area, center_x, base_y, '_', outline);
            put_if_inside(buf, area, center_x + 1, base_y, '/', outline);
            put_if_inside(buf, area, center_x - 1, base_y - 1, '.', shade);
        }
        _ => {
            put_if_inside(buf, area, center_x - 1, base_y - 2, '/', outline);
            put_if_inside(buf, area, center_x, base_y - 2, '\\', outline);
            put_if_inside(buf, area, center_x - 2, base_y - 1, '<', outline);
            put_if_inside(buf, area, center_x - 1, base_y, '\\', outline);
            put_if_inside(buf, area, center_x, base_y, '_', outline);
            put_if_inside(buf, area, center_x + 1, base_y, '/', outline);
            put_if_inside(buf, area, center_x - 1, base_y - 1, '.', shade);
        }
    }
}

fn put_if_inside(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    x: i32,
    y: i32,
    ch: char,
    style: Style,
) {
    let x_min = area.x as i32;
    let x_max = (area.x + area.width) as i32;
    let y_min = area.y as i32;
    let y_max = (area.y + area.height) as i32;
    if x >= x_min && x < x_max && y >= y_min && y < y_max {
        put(buf, x as u16, y as u16, ch, style);
    }
}

fn draw_token_forest_logo(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    ground_y: u16,
    frame_index: u64,
) {
    const LOGO_LARGE: [&str; 4] = [
        " _____     _              ___                    _   ",
        "|_   _|__ | |_____ _ _   | __|__ _ _ _ ___ _____| |_ ",
        "  | |/ _ \\| / / -_) ' \\  | _/ _ \\ '_/ -_|_-<_-<|  _|",
        "  |_|\\___/|_\\_\\___|_||_| |_|\\___/_| \\___/__/__/ \\__|",
    ];

    let top = area.y.saturating_add(1);
    if top >= ground_y {
        return;
    }

    let pulse = 0.8 + 0.2 * ((frame_index as f32 * 0.08).sin() * 0.5 + 0.5);
    let shadow_style = Style::default()
        .fg(scale_rgb((24, 56, 34), pulse * 0.9))
        .bg(Color::Black);
    let front_styles = [
        Style::default()
            .fg(scale_rgb((198, 246, 208), pulse * 1.03))
            .bg(Color::Black),
        Style::default()
            .fg(scale_rgb((158, 222, 172), pulse * 1.01))
            .bg(Color::Black),
        Style::default()
            .fg(scale_rgb((114, 191, 132), pulse * 0.99))
            .bg(Color::Black),
        Style::default()
            .fg(scale_rgb((84, 154, 100), pulse * 0.97))
            .bg(Color::Black),
    ];

    let large_width = LOGO_LARGE.iter().map(|line| line.len()).max().unwrap_or(0) as u16;
    let large_height = LOGO_LARGE.len() as u16;
    let can_draw_large = area.width >= large_width.saturating_add(4)
        && ground_y >= top.saturating_add(large_height).saturating_add(1);

    if can_draw_large {
        let x0 = area.x.saturating_add(2);
        for (row, line) in LOGO_LARGE.iter().enumerate() {
            let y = top + row as u16;
            for (col, ch) in line.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                put_if_inside(
                    buf,
                    area,
                    x0 as i32 + col as i32 + 2,
                    y as i32 + 1,
                    ch,
                    shadow_style,
                );
                put_if_inside(
                    buf,
                    area,
                    x0 as i32 + col as i32,
                    y as i32,
                    ch,
                    front_styles[row.min(front_styles.len() - 1)],
                );
            }
        }
        return;
    }

    let compact = "Token Forest";
    let x0 = area.x.saturating_add(2);
    for (col, ch) in compact.chars().enumerate() {
        if ch == ' ' {
            continue;
        }
        put_if_inside(
            buf,
            area,
            x0 as i32 + col as i32 + 1,
            top as i32 + 1,
            ch,
            shadow_style,
        );
        put_if_inside(
            buf,
            area,
            x0 as i32 + col as i32,
            top as i32,
            ch,
            Style::default()
                .fg(scale_rgb((158, 222, 172), pulse))
                .bg(Color::Black),
        );
    }
}

fn scale_rgb(color: (u8, u8, u8), scale: f32) -> Color {
    let scale = scale.clamp(0.0, 1.35);
    let r = ((color.0 as f32) * scale).clamp(0.0, 255.0).round() as u8;
    let g = ((color.1 as f32) * scale).clamp(0.0, 255.0).round() as u8;
    let b = ((color.2 as f32) * scale).clamp(0.0, 255.0).round() as u8;
    Color::Rgb(r, g, b)
}

fn lerp_f32(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}

fn rgb_lerp(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let r = lerp_f32(from.0 as f32, to.0 as f32, t).round() as u8;
    let g = lerp_f32(from.1 as f32, to.1 as f32, t).round() as u8;
    let b = lerp_f32(from.2 as f32, to.2 as f32, t).round() as u8;
    Color::Rgb(r, g, b)
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
