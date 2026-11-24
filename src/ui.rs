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

/// How often live connections are re-read while no key is pressed.
const CONNECTION_REFRESH: Duration = Duration::from_secs(2);

const INFO_TEXT_TABS: &str =
    "(Esc) quit | (↑/↓) move | (←/→, tab) switch tab | (enter) connect or go to tab | (ctrl+r) reload";

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
    pub terminal_tabs: bool,
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
    /// The terminal emulator, for opening and switching tabs.
    terminal: session::TerminalKind,
    /// Interactive ssh processes at the last refresh.
    connections: Vec<session::Connection>,
    /// The tab chooser shown for a host that already has a connection.
    popup: Option<Popup>,
    /// A message shown in the footer until the next key press.
    status: Option<String>,
    /// The hosts of the current tab, filtered by the search input.
    hosts: Searchable<ssh::Host>,
    table_columns_constraints: Vec<Constraint>,
    page_step: usize,

    palette: tailwind::Palette,
}

/// The tab chooser for a host that already has connections.
struct Popup {
    host: ssh::Host,
    /// Number of terminals holding a connection to the host.
    connections: usize,
    /// Tabs found for those connections, in the order offered.
    tabs: Vec<session::Tab>,
    /// Why the terminal could not be asked for its tabs, if it could not.
    error: Option<String>,
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
            terminal: session::TerminalKind::detect(),
            connections: Vec::new(),
            popup: None,
            status: None,
        };
        app.refresh_connections();
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

            if self.config.terminal_tabs && !event::poll(CONNECTION_REFRESH)? {
                self.refresh_connections();
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
        self.status = None;

        if self.popup.is_some() {
            if is_ctrl_pressed && key.code == Char('c') {
                return Ok(AppKeyAction::Stop);
            }
            self.on_popup_key(key.code);
            return Ok(AppKeyAction::Ok);
        }

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

                if self.config.terminal_tabs && self.terminal != session::TerminalKind::Other {
                    let host = host.clone();
                    self.connect_or_choose_tab(&host);
                    return Ok(AppKeyAction::Ok);
                }

                restore_terminal(terminal).expect("Failed to restore terminal");

                if let Some(template) = &self.config.command_template_on_session_start {
                    host.spawn_command_template(template)?;
                }

                host.spawn_command_template(&self.config.command_template)?;

                if let Some(template) = &self.config.command_template_on_session_end {
                    host.spawn_command_template(template)?;
                }

                setup_terminal(terminal).expect("Failed to setup terminal");
                self.refresh_connections();

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

    fn refresh_connections(&mut self) {
        if self.config.terminal_tabs {
            self.connections = session::live_connections();
        }
    }

    /// Number of terminals holding a connection to `host`.
    fn connection_count(&self, host: &ssh::Host) -> usize {
        session::ttys_for(&self.connections, &host.name, &host.destination).len()
    }

    /// The host name as shown in the Name column. In tabs mode every name gets
    /// a three-column prefix: `●N ` with the number of connections, or blanks.
    fn name_cell(&self, host: &ssh::Host) -> String {
        if !self.config.terminal_tabs {
            return grouping::display_name(&host.name);
        }
        let prefix = match self.connection_count(host) {
            0 => "   ".to_string(),
            n @ 1..=9 => format!("●{n} "),
            _ => "●+ ".to_string(),
        };
        format!("{prefix}{}", host.name)
    }

    /// Opens a new tab for a host without a connection, or shows the tab
    /// chooser for one that has some.
    fn connect_or_choose_tab(&mut self, host: &ssh::Host) {
        let ttys = session::ttys_for(&self.connections, &host.name, &host.destination);
        if ttys.is_empty() {
            self.open_new_tab(host);
            return;
        }

        let (tabs, error) = match session::tabs_for(self.terminal, &ttys, &host.name) {
            Ok(tabs) => (tabs, None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };
        self.popup = Some(Popup {
            host: host.clone(),
            connections: ttys.len(),
            tabs,
            error,
        });
    }

    fn open_new_tab(&mut self, host: &ssh::Host) {
        let result = host
            .render_command_template(&self.config.command_template)
            .and_then(|command| session::open_new_tab(self.terminal, &command));
        if let Err(err) = result {
            self.status = Some(format!("Could not open a tab: {err}"));
        }
    }

    fn on_popup_key(&mut self, code: KeyCode) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        let jump_to = |tab: &session::Tab| session::jump(&tab.target);

        let result = match code {
            KeyCode::Esc => Ok(()),
            KeyCode::Char('n') => {
                self.open_new_tab(&popup.host);
                Ok(())
            }
            KeyCode::Enter => {
                if let Some(tab) = popup.tabs.first() {
                    jump_to(tab)
                } else {
                    self.open_new_tab(&popup.host);
                    Ok(())
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = c as usize - '1' as usize;
                if let Some(tab) = popup.tabs.get(index) {
                    jump_to(tab)
                } else {
                    self.popup = Some(popup);
                    return;
                }
            }
            _ => {
                self.popup = Some(popup);
                return;
            }
        };

        if let Err(err) = result {
            self.status = Some(format!("Could not switch to the tab: {err}"));
        }
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
            .map(|d| self.name_cell(d))
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

    if let Some(popup) = &app.popup {
        render_popup(f, popup, &app.palette);
        return;
    }

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
            app.name_cell(host),
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

fn popup_lines(popup: &Popup) -> Vec<Line<'static>> {
    let key = Style::new().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();

    if popup.tabs.is_empty() {
        lines.push(Line::from("No tab found for it in this terminal."));
        if let Some(error) = &popup.error {
            lines.push(Line::from(error.clone()).style(Style::new().fg(tailwind::RED.c400)));
        }
    }
    for (i, tab) in popup.tabs.iter().take(9).enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("[{}] ", i + 1), key),
            Span::raw(tab.label.clone()),
        ]));
    }

    lines.push(Line::from(""));
    let enter = if popup.tabs.is_empty() {
        "new connection"
    } else {
        "go to tab 1"
    };
    for (k, what) in [
        ("[Enter] ", enter),
        ("[n]     ", "new connection"),
        ("[Esc]   ", "cancel"),
    ] {
        lines.push(Line::from(vec![Span::styled(k, key), Span::raw(what)]));
    }
    lines
}

fn render_popup(f: &mut Frame, popup: &Popup, palette: &tailwind::Palette) {
    let title = format!(
        " {}: {} connection{} ",
        popup.host.name,
        popup.connections,
        if popup.connections == 1 { "" } else { "s" }
    );
    let lines = popup_lines(popup);

    let content_width = lines
        .iter()
        .map(Line::width)
        .chain(std::iter::once(UnicodeWidthStr::width(title.as_str())))
        .max()
        .unwrap_or(0);
    let area = f.area();
    let width = u16::try_from(content_width + 6)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::new().fg(palette.c400))
                .border_type(BorderType::Rounded)
                .padding(Padding::horizontal(2)),
        ),
        rect,
    );
}

fn render_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let text = match (&app.status, app.config.terminal_tabs) {
        (Some(status), _) => status.as_str(),
        (None, true) => INFO_TEXT_TABS,
        (None, false) => INFO_TEXT,
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
            terminal_tabs: false,
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

    fn tabs_app() -> App {
        let mut config = test_config();
        config.config_paths = vec![crate::test_support::testdata("groups.conf")
            .to_string_lossy()
            .into_owned()];
        config.terminal_tabs = true;
        let mut app = App::new(&config).unwrap();
        // Never open or switch real tabs from a test, whatever terminal runs it.
        app.terminal = session::TerminalKind::Other;
        app
    }

    fn connection(destination: &str, tty: &str) -> session::Connection {
        session::Connection {
            destination: destination.to_string(),
            tty: tty.to_string(),
        }
    }

    fn render(app: &mut App, width: u16) -> String {
        let mut terminal = Terminal::new(backend::TestBackend::new(width, 20)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_tabs_mode_counts_connections_per_host() {
        let mut app = tabs_app();
        app.connections = vec![
            connection("acme-web-b", "/dev/pts/3"),
            // The same host by its HostName, in another terminal.
            connection("10.0.0.2", "/dev/pts/4"),
            // A second ssh in the same terminal is not a second tab.
            connection("acme-web-b", "/dev/pts/3"),
            connection("acme-web-g", "/dev/pts/5"),
        ];

        let screen = render(&mut app, 120);
        assert!(screen.contains("●2 acme-web-b"), "{screen}");
        assert!(screen.contains("●1 acme-web-g"), "{screen}");
        assert!(screen.contains("   zeta-web-b"), "{screen}");
        assert!(!screen.contains("● zeta"), "{screen}");
        assert!(screen.contains("(enter) connect or go to tab"), "{screen}");
    }

    #[test]
    fn test_name_cell_without_tabs_mode_is_unchanged() {
        let mut app = App::new(&test_config()).unwrap();
        app.connections = vec![connection("match1", "/dev/pts/3")];
        let host = app.all_hosts[0].clone();
        assert_eq!(app.name_cell(&host), "match1");
    }

    #[test]
    fn test_popup_lists_tabs_and_handles_keys() {
        let mut app = tabs_app();
        let host = app.all_hosts[1].clone();
        app.popup = Some(Popup {
            host,
            connections: 2,
            tabs: vec![
                session::Tab {
                    label: "tab 2: deploy@acme".into(),
                    target: session::JumpTarget::WezTermPane {
                        tab_id: 7,
                        pane_id: 7,
                    },
                },
                session::Tab {
                    label: "tab 5: logs".into(),
                    target: session::JumpTarget::WezTermPane {
                        tab_id: 9,
                        pane_id: 9,
                    },
                },
            ],
            error: None,
        });

        let screen = render(&mut app, 120);
        assert!(screen.contains("acme-web-b: 2 connections"), "{screen}");
        assert!(screen.contains("[1] tab 2: deploy@acme"), "{screen}");
        assert!(screen.contains("[2] tab 5: logs"), "{screen}");
        assert!(screen.contains("[n]"), "{screen}");

        // A number without a tab and an unrelated key keep the popup open.
        app.on_popup_key(KeyCode::Char('7'));
        app.on_popup_key(KeyCode::Char('x'));
        assert!(app.popup.is_some());

        app.on_popup_key(KeyCode::Esc);
        assert!(app.popup.is_none());
        assert!(app.status.is_none());
    }

    #[test]
    fn test_popup_without_tabs_offers_new_connection() {
        let mut app = tabs_app();
        let host = app.all_hosts[0].clone();
        app.popup = Some(Popup {
            host,
            connections: 1,
            tabs: Vec::new(),
            error: Some("gdbus failed".into()),
        });

        let screen = render(&mut app, 120);
        assert!(screen.contains("No tab found"), "{screen}");
        assert!(screen.contains("gdbus failed"), "{screen}");
        assert!(screen.contains("[Enter] new connection"), "{screen}");

        // Enter opens a new tab; this terminal cannot, which is reported.
        app.on_popup_key(KeyCode::Enter);
        assert!(app.popup.is_none());
        let status = app.status.clone().unwrap();
        assert!(status.contains("cannot open tabs"), "{status}");
    }

    #[test]
    fn test_enter_on_host_with_connection_opens_popup() {
        let mut app = tabs_app();
        app.connections = vec![connection("dev-web", "/dev/pts/3")];
        let host = app.all_hosts[0].clone();
        assert_eq!(host.name, "dev-web");

        app.connect_or_choose_tab(&host);
        let popup = app.popup.as_ref().unwrap();
        assert_eq!(popup.connections, 1);
        assert_eq!(popup.tabs, Vec::new());
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
