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

const MAX_TOKEN_RATE: f32 = 2400.0;

fn main() -> Result<()> {
    let mut terminal = setup_terminal()?;
    let run_result = run_app(&mut terminal);
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

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let tick_rate = Duration::from_millis(80);
    let mut app = App::new();

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
    started_at: Instant,
    last_tick: Instant,
    frame_index: u64,
    token_rate: f32,
    smoothed_rate: f32,
    rain_intensity: f32,
}

impl App {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            running: true,
            started_at: now,
            last_tick: now,
            frame_index: 0,
            token_rate: 0.0,
            smoothed_rate: 0.0,
            rain_intensity: 0.0,
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
            self.running = false;
        }
    }

    fn tick(&mut self) {
        self.last_tick = Instant::now();
        self.frame_index = self.frame_index.saturating_add(1);

        let elapsed = self.started_at.elapsed().as_secs_f32();
        let wave = 900.0
            + elapsed.sin() * 360.0
            + (elapsed * 0.42).sin() * 260.0
            + (elapsed * 0.09).cos() * 180.0;
        let burst = if (elapsed as i32 % 19) < 4 {
            650.0
        } else {
            0.0
        };
        self.token_rate = (wave + burst).max(0.0);
        self.smoothed_rate = self.smoothed_rate * 0.86 + self.token_rate * 0.14;
        self.rain_intensity = (self.smoothed_rate / MAX_TOKEN_RATE).clamp(0.0, 1.0);
    }
}

fn render(frame: &mut Frame, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(frame.area());

    let header = format!(
        "Cyber Bonsai | q/esc quit | token est {:>5.0}/s | rain {:>3.0}%",
        app.smoothed_rate,
        app.rain_intensity * 100.0
    );
    let status =
        Paragraph::new(header).block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(status, areas[0]);
    frame.render_widget(RainForestWidget { app }, areas[1]);
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
        let phase = self.app.started_at.elapsed().as_secs_f32();
        for i in 0..tree_count {
            let sway = ((phase * 1.4 + i as f32).sin() * 2.0) as i16;
            let base_x = area.x + i * step + step / 2;
            let trunk_x = base_x
                .saturating_add_signed(sway)
                .clamp(area.x + 1, area.x + area.width - 2);
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
