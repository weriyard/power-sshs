use anyhow::Result;
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
#[allow(clippy::wildcard_imports)]
use ratatui::{prelude::*, widgets::*};
use std::{
    cell::RefCell,
    cmp::{max, min},
    collections::HashSet,
    io,
    rc::Rc,
    time::Duration,
};
use style::palette::tailwind;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use unicode_width::UnicodeWidthStr;

use crate::{
    grouping::{self, Slot, Tab},
    searchable::Searchable,
    session, ssh,
};

/// How often the live tmux sessions are re-read while no key is pressed.
const SESSION_REFRESH: Duration = Duration::from_secs(2);

const INFO_TEXT_TMUX: &str =
    "(Esc) quit | (↑/↓) move | (←/→, tab) switch tab | (enter) open session | (ctrl+r) reload";

const INFO_TEXT: &str =
    "(Esc) quit | (↑/↓) move | (←/→, tab) switch tab | (enter) select | (ctrl+r) reload";

#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct AppConfig {
    pub config_paths: Vec<String>,

    pub search_filter: Option<String>,
    pub color: String,
    pub sort_by_name: bool,
    pub sort_by_score: bool,
    pub show_proxy_command: bool,
    pub initial_tab: Tab,

    pub command_template: String,
    pub command_template_on_session_start: Option<String>,
    pub command_template_on_session_end: Option<String>,
    pub exit_after_ssh_session_ends: bool,
    pub tmux_sessions: bool,
}

pub struct App {
    config: AppConfig,

    search: Input,

    /// Selection as an index into the filtered host list.
    table_state: TableState,
    /// Selection and scroll offset in the rendered table, which also holds
    /// group header rows.
    display_state: TableState,
    /// Every host from the configuration, in display order.
    all_hosts: Vec<ssh::Host>,
    tab: Tab,
    /// tmux sessions alive at the last refresh, by session name.
    live_sessions: HashSet<String>,
    /// The hosts of the current tab, filtered by the search input.
    hosts: Searchable<ssh::Host>,
    table_columns_constraints: Vec<Constraint>,
    page_step: usize,

    palette: tailwind::Palette,
}

#[derive(PartialEq)]
enum AppKeyAction {
    Ok,
    Stop,
    Continue,
}

impl App {
    /// # Errors
    ///
    /// Will return `Err` if the SSH configuration file cannot be parsed.
    pub fn new(config: &AppConfig) -> Result<App> {
        let hosts = load_hosts(config)?;

        let search_input = config.search_filter.clone().unwrap_or_default();

        let mut app = App {
            config: config.clone(),

            search: search_input.clone().into(),

            table_state: TableState::default().with_selected(0),
            display_state: TableState::default(),
            table_columns_constraints: Vec::new(),
            page_step: 21,
            palette: palette_by_name(&config.color)?,

            hosts: Searchable::new(
                config.sort_by_score,
                tab_hosts(&hosts, config.initial_tab),
                &search_input,
            ),
            all_hosts: hosts,
            tab: config.initial_tab,
            live_sessions: HashSet::new(),
        };
        app.refresh_live_sessions();
        app.calculate_table_columns_constraints();

        Ok(app)
    }

    /// # Errors
    ///
    /// Will return `Err` if the terminal cannot be configured.
    pub fn start(&mut self) -> Result<()> {
        let stdout = io::stdout().lock();
        let backend = CrosstermBackend::new(stdout);
        let terminal = Rc::new(RefCell::new(Terminal::new(backend)?));

        setup_terminal(&terminal)?;

        // create app and run it
        let res = self.run(&terminal);

        restore_terminal(&terminal)?;

        if let Err(err) = res {
            println!("{err:?}");
        }

        Ok(())
    }

    fn run<B>(&mut self, terminal: &Rc<RefCell<Terminal<B>>>) -> Result<()>
    where
        B: Backend + std::io::Write,
        <B as Backend>::Error: Send + Sync + 'static,
    {
        loop {
            terminal.borrow_mut().draw(|f| ui(f, self))?;

            if self.config.tmux_sessions && !event::poll(SESSION_REFRESH)? {
                self.refresh_live_sessions();
                continue;
            }

            let ev = event::read()?;

            if let Event::Key(key) = ev {
                if key.kind == KeyEventKind::Press {
                    let action = self.on_key_press(terminal, key)?;
                    match action {
                        AppKeyAction::Ok => continue,
                        AppKeyAction::Stop => break,
                        AppKeyAction::Continue => {}
                    }
                }

                self.handle_search_event(&ev);
            }
        }

        Ok(())
    }

    fn on_key_press<B>(
        &mut self,
        terminal: &Rc<RefCell<Terminal<B>>>,
        key: KeyEvent,
    ) -> Result<AppKeyAction>
    where
        B: Backend + std::io::Write,
        <B as Backend>::Error: Send + Sync + 'static,
    {
        #[allow(clippy::enum_glob_use)]
        use KeyCode::*;

        let is_ctrl_pressed = key.modifiers.contains(KeyModifiers::CONTROL);

        if is_ctrl_pressed {
            let action = self.on_key_press_ctrl(key);
            if action != AppKeyAction::Continue {
                return Ok(action);
            }
        }

        match key.code {
            Esc => return Ok(AppKeyAction::Stop),
            Right | KeyCode::Tab => self.show_tab(self.tab.next()),
            Left | KeyCode::BackTab => self.show_tab(self.tab.previous()),
            Down => self.next(),
            Up => self.previous(),
            Home => self.table_state.select(Some(0)),
            End => self
                .table_state
                .select(Some(self.hosts.len().saturating_sub(1))),
            PageDown => {
                let i = self.table_state.selected().unwrap_or(0);
                let target = min(
                    i.saturating_add(self.page_step),
                    self.hosts.len().saturating_sub(1),
                );

                self.table_state.select(Some(target));
            }
            PageUp => {
                let i = self.table_state.selected().unwrap_or(0);
                let target = max(i.saturating_sub(self.page_step), 0);

                self.table_state.select(Some(target));
            }
            Enter => {
                let selected = self.table_state.selected().unwrap_or(0);
                if selected >= self.hosts.len() {
                    return Ok(AppKeyAction::Ok);
                }

                let host: &ssh::Host = &self.hosts[selected];

                if self.config.tmux_sessions && session::inside_tmux() {
                    // The client switches to the host's session; this one keeps running.
                    let command = host.render_command_template(&self.config.command_template)?;
                    session::open(&host.name, &command)?;
                    self.refresh_live_sessions();
                    return Ok(AppKeyAction::Ok);
                }

                restore_terminal(terminal).expect("Failed to restore terminal");

                if let Some(template) = &self.config.command_template_on_session_start {
                    host.spawn_command_template(template)?;
                }

                if self.config.tmux_sessions {
                    let command = host.render_command_template(&self.config.command_template)?;
                    session::open(&host.name, &command)?;
                } else {
                    host.spawn_command_template(&self.config.command_template)?;
                }

                if let Some(template) = &self.config.command_template_on_session_end {
                    host.spawn_command_template(template)?;
                }

                setup_terminal(terminal).expect("Failed to setup terminal");
                self.refresh_live_sessions();

                if self.config.exit_after_ssh_session_ends {
                    return Ok(AppKeyAction::Stop);
                }
            }
            _ => return Ok(AppKeyAction::Continue),
        }

        Ok(AppKeyAction::Ok)
    }

    fn on_key_press_ctrl(&mut self, key: KeyEvent) -> AppKeyAction {
        #[allow(clippy::enum_glob_use)]
        use KeyCode::*;

        match key.code {
            Char('c') => AppKeyAction::Stop,
            Char('j' | 'n') => {
                self.next();
                AppKeyAction::Ok
            }
            Char('k' | 'p') => {
                self.previous();
                AppKeyAction::Ok
            }
            Char('r') => {
                self.reload_hosts();
                AppKeyAction::Ok
            }
            _ => AppKeyAction::Continue,
        }
    }

    /// Updates the search input from a terminal event, re-filters the host
    /// list, and keeps the table selection valid.
    ///
    /// When the search text actually changes, the selection resets to the
    /// top result instead of keeping its previous numeric index: the old
    /// index could otherwise point at an unrelated host in the newly
    /// filtered list, making an apparently-unmatched host look selected.
    fn handle_search_event(&mut self, ev: &Event) {
        let search_value_before = self.search.value().to_string();
        self.search.handle_event(ev);

        if self.search.value() == search_value_before {
            let selected = self.table_state.selected().unwrap_or(0);
            if selected >= self.hosts.len() {
                self.table_state.select(Some(match self.hosts.len() {
                    0 => 0,
                    _ => self.hosts.len() - 1,
                }));
            }
        } else {
            self.hosts.search(self.search.value());
            self.table_state.select(Some(0));
        }
    }

    /// Re-reads the configuration files. Keeps the current list when a file
    /// no longer parses.
    fn reload_hosts(&mut self) {
        if let Ok(hosts) = load_hosts(&self.config) {
            self.all_hosts = hosts;
            self.show_tab(self.tab);
            self.calculate_table_columns_constraints();
        }
    }

    fn refresh_live_sessions(&mut self) {
        if self.config.tmux_sessions {
            self.live_sessions = session::live_sessions();
        }
    }

    /// The host name as shown in the Name column. In tmux mode a host with a
    /// live session gets [`session::LIVE_MARKER`] in place of its indent.
    fn name_cell(&self, name: &str) -> String {
        if !self.config.tmux_sessions {
            return grouping::display_name(name);
        }
        let prefix = if self.live_sessions.contains(&session::session_name(name)) {
            session::LIVE_MARKER
        } else {
            grouping::INDENT
        };
        format!("{prefix}{name}")
    }

    /// Switches to `tab`, keeping the search input, and selects its first host.
    fn show_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.hosts = Searchable::new(
            self.config.sort_by_score,
            tab_hosts(&self.all_hosts, tab),
            self.search.value(),
        );
        self.table_state.select(Some(0));
    }

    fn next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if self.hosts.is_empty() || i >= self.hosts.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn previous(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if self.hosts.is_empty() {
                    0
                } else if i == 0 {
                    self.hosts.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn calculate_table_columns_constraints(&mut self) {
        let mut lengths = Vec::new();

        let name_len = self
            .all_hosts
            .iter()
            .map(|d| self.name_cell(&d.name))
            .map(|name| UnicodeWidthStr::width(name.as_str()))
            .max()
            .unwrap_or(0);
        lengths.push(name_len);

        let aliases_len = self
            .all_hosts
            .iter()
            .map(|d| d.aliases.as_str())
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(aliases_len);

        let user_len = self
            .all_hosts
            .iter()
            .map(|d| match &d.user {
                Some(user) => user.as_str(),
                None => "",
            })
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(user_len);

        let destination_len = self
            .all_hosts
            .iter()
            .map(|d| d.destination.as_str())
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(destination_len);

        let port_len = self
            .all_hosts
            .iter()
            .map(|d| match &d.port {
                Some(port) => port.as_str(),
                None => "",
            })
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(port_len);

        if self.config.show_proxy_command {
            let proxy_len = self
                .all_hosts
                .iter()
                .map(|d| match &d.proxy_command {
                    Some(proxy) => proxy.as_str(),
                    None => "",
                })
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(0);
            lengths.push(proxy_len);
        }

        let mut new_constraints = vec![
            // +1 for padding
            Constraint::Length(u16::try_from(lengths[0]).unwrap_or_default() + 1),
        ];
        new_constraints.extend(
            lengths
                .iter()
                .skip(1)
                .map(|len| Constraint::Min(u16::try_from(*len).unwrap_or_default() + 1)),
        );

        self.table_columns_constraints = new_constraints;
    }
}

fn tab_hosts(hosts: &[ssh::Host], tab: Tab) -> Vec<ssh::Host> {
    hosts
        .iter()
        .filter(|host| tab.contains(&host.name))
        .cloned()
        .collect()
}

fn load_hosts(config: &AppConfig) -> Result<Vec<ssh::Host>> {
    let mut hosts = Vec::new();

    for path in &config.config_paths {
        let parsed_hosts = match ssh::parse_config(path) {
            Ok(hosts) => hosts,
            Err(err) => {
                if let ssh::ParseConfigError::Io(io_err) = &err {
                    if io_err.kind() == std::io::ErrorKind::NotFound {
                        if path == "/etc/ssh/ssh_config" {
                            // Ignore missing system-wide SSH configuration file
                            continue;
                        }

                        anyhow::bail!(
                            "SSH configuration file not found: {path}\nCreate it, or pass a different path with -c/--config."
                        );
                    }
                }

                anyhow::bail!("Failed to parse SSH configuration file: {err:?}");
            }
        };

        hosts.extend(parsed_hosts);
    }

    if config.sort_by_name {
        hosts.sort_by_key(|host| host.name.to_lowercase());
    }

    Ok(grouping::group_hosts(hosts))
}

fn palette_by_name(name: &str) -> Result<tailwind::Palette> {
    let palette = match name.to_lowercase().as_str() {
        "slate" => tailwind::SLATE,
        "gray" => tailwind::GRAY,
        "zinc" => tailwind::ZINC,
        "neutral" => tailwind::NEUTRAL,
        "stone" => tailwind::STONE,
        "red" => tailwind::RED,
        "orange" => tailwind::ORANGE,
        "amber" => tailwind::AMBER,
        "yellow" => tailwind::YELLOW,
        "lime" => tailwind::LIME,
        "green" => tailwind::GREEN,
        "emerald" => tailwind::EMERALD,
        "teal" => tailwind::TEAL,
        "cyan" => tailwind::CYAN,
        "sky" => tailwind::SKY,
        "blue" => tailwind::BLUE,
        "indigo" => tailwind::INDIGO,
        "violet" => tailwind::VIOLET,
        "purple" => tailwind::PURPLE,
        "fuchsia" => tailwind::FUCHSIA,
        "pink" => tailwind::PINK,
        "rose" => tailwind::ROSE,
        _ => anyhow::bail!(
            "Unknown color: {name}\nValid colors: slate, gray, zinc, neutral, stone, red, orange, amber, yellow, lime, green, emerald, teal, cyan, sky, blue, indigo, violet, purple, fuchsia, pink, rose"
        ),
    };

    Ok(palette)
}

fn setup_terminal<B>(terminal: &Rc<RefCell<Terminal<B>>>) -> Result<()>
where
    B: Backend + std::io::Write,
    <B as Backend>::Error: Send + Sync + 'static,
{
    let mut terminal = terminal.borrow_mut();

    // setup terminal
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), Hide, EnterAlternateScreen)?;

    Ok(())
}

fn restore_terminal<B>(terminal: &Rc<RefCell<Terminal<B>>>) -> Result<()>
where
    B: Backend + std::io::Write,
    <B as Backend>::Error: Send + Sync + 'static,
{
    let mut terminal = terminal.borrow_mut();
    terminal.clear()?;

    // restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), Show, LeaveAlternateScreen)?;

    Ok(())
}

fn ui(f: &mut Frame, app: &mut App) {
    let rects = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .split(f.area());

    render_searchbar(f, app, rects[0]);

    render_table(f, app, rects[1]);

    render_footer(f, app, rects[2]);

    let mut cursor_position = rects[0].as_position();
    cursor_position.x += u16::try_from(app.search.cursor()).unwrap_or_default() + 4;
    cursor_position.y += 1;

    f.set_cursor_position(cursor_position);
}

fn render_searchbar(f: &mut Frame, app: &mut App, area: Rect) {
    let info_footer = Paragraph::new(Line::from(app.search.value())).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(app.palette.c400))
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(3)),
    );
    f.render_widget(info_footer, area);
}

fn render_table(f: &mut Frame, app: &mut App, area: Rect) {
    // The visible row count: the area minus the two border lines and the header.
    app.page_step = max(usize::from(area.height.saturating_sub(3)), 1);

    let header_style = Style::default().fg(tailwind::CYAN.c500);
    let selected_style = Style::default().add_modifier(Modifier::REVERSED);

    let mut header_names = vec!["Name", "Aliases", "User", "Destination", "Port"];
    if app.config.show_proxy_command {
        header_names.push("Proxy");
    }

    let header = header_names
        .iter()
        .copied()
        .map(Cell::from)
        .collect::<Row>()
        .style(header_style)
        .height(1);

    let group_style = Style::default()
        .fg(app.palette.c300)
        .add_modifier(Modifier::BOLD);
    let column_count = header_names.len();

    let selected_host = app.table_state.selected().unwrap_or(0);
    let mut selected_row = None;
    let mut rows = Vec::new();
    let mut previous_group: Option<Option<String>> = None;

    for (index, host) in app.hosts.iter().enumerate() {
        let group = grouping::group_key(&host.name);
        if previous_group.as_ref() != Some(&group) {
            if let Some(name) = &group {
                let mut cells = vec![Cell::from(name.to_uppercase())];
                cells.extend((1..column_count).map(|_| Cell::from("")));
                rows.push(Row::new(cells).style(group_style));
            }
            previous_group = Some(group);
        }

        if index == selected_host {
            selected_row = Some(rows.len());
        }

        let row_style = host_style(&host.name);

        let mut content = vec![
            app.name_cell(&host.name),
            host.aliases.clone(),
            host.user.clone().unwrap_or_default(),
            host.destination.clone(),
            host.port.clone().unwrap_or_default(),
        ];
        if app.config.show_proxy_command {
            content.push(host.proxy_command.clone().unwrap_or_default());
        }

        rows.push(
            content
                .iter()
                .map(|content| Cell::from(Text::from(content.clone())))
                .collect::<Row>()
                .style(row_style),
        );
    }

    app.display_state.select(selected_row);

    let bar = " █ ";
    let t = Table::new(rows, app.table_columns_constraints.clone())
        .header(header)
        .row_highlight_style(selected_style)
        .highlight_symbol(Text::from(vec![
            "".into(),
            bar.into(),
            bar.into(),
            "".into(),
        ]))
        .highlight_spacing(HighlightSpacing::Always)
        .block(
            Block::default()
                .title(tabs_line(app))
                .borders(Borders::ALL)
                .border_style(Style::new().fg(app.palette.c400))
                .border_type(BorderType::Rounded),
        );

    f.render_stateful_widget(t, area, &mut app.display_state);
}

const COLOR_DEV: Color = tailwind::ORANGE.c400;
const COLOR_B: Color = tailwind::BLUE.c400;
const COLOR_G: Color = tailwind::GREEN.c400;
const COLOR_ALL: Color = tailwind::YELLOW.c400;

/// Row colour of a host: by slot first, then orange for the `dev` group.
fn host_style(name: &str) -> Style {
    match grouping::slot(name) {
        Slot::B => Style::default().fg(COLOR_B),
        Slot::G => Style::default().fg(COLOR_G),
        Slot::Other if Tab::Dev.contains(name) => Style::default().fg(COLOR_DEV),
        Slot::Other => Style::default(),
    }
}

fn tab_color(tab: Tab) -> Color {
    match tab {
        Tab::Dev => COLOR_DEV,
        Tab::B => COLOR_B,
        Tab::G => COLOR_G,
        Tab::All => COLOR_ALL,
    }
}

/// The tab bar shown in the top border of the host table, with the number
/// of hosts in each tab.
fn tabs_line(app: &App) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];

    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", Style::new().fg(app.palette.c400)));
        }

        let count = app
            .all_hosts
            .iter()
            .filter(|host| tab.contains(&host.name))
            .count();

        let mut style = Style::new().fg(tab_color(*tab));
        if *tab == app.tab {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }

        spans.push(Span::styled(format!(" {} ({count}) ", tab.label()), style));
    }

    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn render_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let text = if app.config.tmux_sessions {
        INFO_TEXT_TMUX
    } else {
        INFO_TEXT
    };
    let info_footer = Paragraph::new(Line::from(text)).centered().block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(app.palette.c400))
            .border_type(BorderType::Rounded),
    );
    f.render_widget(info_footer, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AppConfig {
        AppConfig {
            config_paths: vec![crate::test_support::testdata("search_selection.conf")
                .to_string_lossy()
                .into_owned()],
            search_filter: None,
            color: "blue".to_string(),
            sort_by_name: false,
            sort_by_score: false,
            show_proxy_command: false,
            initial_tab: Tab::All,
            command_template: r#"ssh "{{{name}}}""#.to_string(),
            command_template_on_session_start: None,
            command_template_on_session_end: None,
            exit_after_ssh_session_ends: false,
            tmux_sessions: false,
        }
    }

    fn type_char(app: &mut App, c: char) {
        let ev = Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        app.handle_search_event(&ev);
    }

    #[test]
    fn test_reload_hosts_picks_up_file_changes() {
        let path = std::env::temp_dir().join("sshs_test_reload.conf");
        std::fs::write(&path, "Host first\n  Hostname first.example.com\n").unwrap();

        let mut config = test_config();
        config.config_paths = vec![path.to_string_lossy().into_owned()];

        let mut app = App::new(&config).unwrap();
        assert_eq!(app.hosts.len(), 1);

        std::fs::write(
            &path,
            "Host first\n  Hostname first.example.com\nHost second\n  Hostname second.example.com\n",
        )
        .unwrap();
        app.reload_hosts();
        std::fs::remove_file(&path).ok();

        assert_eq!(app.hosts.len(), 2);
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn test_table_columns_constraints_are_computed() {
        let app = App::new(&test_config()).unwrap();

        assert_eq!(app.table_columns_constraints.len(), 5);
        assert!(matches!(
            app.table_columns_constraints[0],
            Constraint::Length(len) if len > 1
        ));
    }

    #[test]
    fn test_hosts_are_grouped_under_client_headers() {
        let mut config = test_config();
        config.config_paths = vec![crate::test_support::testdata("groups.conf")
            .to_string_lossy()
            .into_owned()];
        let mut app = App::new(&config).unwrap();

        let names: Vec<_> = app.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["dev-web", "acme-web-b", "acme-web-g", "zeta-web-b"]
        );

        // Select acme-web-g: host index 2, rendered below the DEV and ACME headers.
        app.next();
        app.next();

        let mut terminal = Terminal::new(backend::TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        assert_eq!(app.display_state.selected(), Some(4));

        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let row_of = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();

        assert!(row_of("DEV") < row_of("dev-web"));
        assert!(row_of("dev-web") < row_of("ACME"));
        assert!(row_of("ACME") < row_of("acme-web-b"));
        assert!(row_of("acme-web-g") < row_of("ZETA"));

        // Hosts are indented under their group header.
        let column_of = |needle: &str| {
            let line = &lines[row_of(needle)];
            line[..line.find(needle).unwrap()].chars().count()
        };
        assert_eq!(
            column_of("acme-web-b"),
            column_of("ACME") + grouping::INDENT.len()
        );
        assert_eq!(
            column_of("zeta-web-b"),
            column_of("ZETA") + grouping::INDENT.len()
        );
        assert!(row_of("ZETA") < row_of("zeta-web-b"));

        let acme_b = row_of("acme-web-b");
        let acme_g = row_of("acme-web-g");
        let byte = lines[acme_b].find("acme-web-b").unwrap();
        let x = u16::try_from(lines[acme_b][..byte].chars().count()).unwrap();
        let fg = |row: usize| buffer[(x, u16::try_from(row).unwrap())].fg;
        assert_eq!(fg(acme_b), tailwind::BLUE.c400);
        // The selected row is reversed, so its colour moves to the background.
        assert!(buffer[(x, u16::try_from(acme_g).unwrap())]
            .modifier
            .contains(Modifier::REVERSED));
        assert_eq!(fg(acme_g), tailwind::GREEN.c400);
        let dev = row_of("dev-web");
        assert_eq!(fg(dev), tailwind::ORANGE.c400);
    }

    #[test]
    fn test_tabs_filter_hosts_and_keep_search() {
        let mut config = test_config();
        config.config_paths = vec![crate::test_support::testdata("groups.conf")
            .to_string_lossy()
            .into_owned()];
        config.initial_tab = Tab::Dev;
        let mut app = App::new(&config).unwrap();

        let names = |app: &App| app.hosts.iter().map(|h| h.name.clone()).collect::<Vec<_>>();

        assert_eq!(names(&app), vec!["dev-web"]);

        app.show_tab(app.tab.next());
        assert_eq!(app.tab, Tab::B);
        assert_eq!(names(&app), vec!["acme-web-b", "zeta-web-b"]);

        app.next();
        app.show_tab(app.tab.next());
        assert_eq!(app.tab, Tab::G);
        assert_eq!(names(&app), vec!["acme-web-g"]);
        assert_eq!(app.table_state.selected(), Some(0));

        app.show_tab(app.tab.next());
        assert_eq!(app.tab, Tab::All);
        assert_eq!(app.hosts.len(), 4);

        // The search input survives a tab switch.
        for c in "acme".chars() {
            type_char(&mut app, c);
        }
        assert_eq!(names(&app), vec!["acme-web-b", "acme-web-g"]);
        app.show_tab(Tab::B);
        assert_eq!(names(&app), vec!["acme-web-b"]);

        let line = tabs_line(&app).to_string();
        assert!(line.contains("devs (1)"), "{line}");
        assert!(line.contains("blue (2)"), "{line}");
        assert!(line.contains("green (1)"), "{line}");
        assert!(line.contains("all (4)"), "{line}");
    }

    #[test]
    fn test_tmux_mode_marks_hosts_with_live_session() {
        let mut config = test_config();
        config.config_paths = vec![crate::test_support::testdata("groups.conf")
            .to_string_lossy()
            .into_owned()];
        config.tmux_sessions = true;
        let mut app = App::new(&config).unwrap();
        app.live_sessions = ["acme-web-b".to_string()].into_iter().collect();

        assert_eq!(app.name_cell("acme-web-b"), "● acme-web-b");
        assert_eq!(app.name_cell("acme-web-g"), "  acme-web-g");

        let mut terminal = Terminal::new(backend::TestBackend::new(120, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol().to_string())
            .collect();

        assert!(screen.contains("● acme-web-b"), "{screen}");
        assert!(!screen.contains("● acme-web-g"), "{screen}");
        assert!(screen.contains("(enter) open session"), "{screen}");
    }

    #[test]
    fn test_name_cell_without_tmux_mode_is_unchanged() {
        let mut app = App::new(&test_config()).unwrap();
        app.live_sessions = ["acme-web-b".to_string()].into_iter().collect();
        assert_eq!(app.name_cell("acme-web-b"), "  acme-web-b");
        assert_eq!(app.name_cell("bastion"), "bastion");
    }

    /// Regression test for <https://github.com/quantumsheep/sshs/issues/120>:
    /// a missing SSH config file used to surface a raw `Io(Os { .. })` debug
    /// error; it should now explain what's missing and how to fix it.
    #[test]
    fn test_missing_config_file_gives_actionable_error() {
        let mut config = test_config();
        config.config_paths = vec!["/nonexistent/path/to/config".to_string()];

        let message = match App::new(&config) {
            Ok(_) => panic!("expected App::new to fail for a missing config file"),
            Err(err) => err.to_string(),
        };

        assert!(
            message.contains("/nonexistent/path/to/config"),
            "error should mention the missing path: {message}"
        );
        assert!(
            !message.contains("Os {"),
            "error should not leak a raw Debug-formatted io::Error: {message}"
        );
    }

    /// Regression test for <https://github.com/quantumsheep/sshs/issues/154>:
    /// typing a search query used to keep the previously selected row index,
    /// which could point at an unrelated host once the list was refiltered.
    #[test]
    fn test_search_resets_selection_to_top_match() {
        let config = test_config();
        let mut app = App::new(&config).unwrap();

        // Sanity check: all 4 hosts are listed in file order before searching.
        assert_eq!(app.hosts.len(), 4);

        // Select the 2nd row ("other"), which won't match the search below.
        app.next();
        assert_eq!(app.table_state.selected(), Some(1));

        for c in "match".chars() {
            type_char(&mut app, c);
        }

        assert_eq!(app.hosts.len(), 3);
        assert_eq!(app.hosts.iter().next().unwrap().name, "match1");

        // The stale index (1) would previously stay selected, highlighting
        // "match2" instead of resetting to the top match "match1".
        assert_eq!(app.table_state.selected(), Some(0));
    }
}
