# power-sshs

A fork of [**sshs**](https://github.com/quantumsheep/sshs) by Nathanael Demacon — a terminal user
interface for SSH that lists the hosts from `~/.ssh/config` and connects to the one you pick.

This fork adds a way to keep a long host list readable: hosts are **grouped by client**,
**coloured by environment**, and split into **tabs**. Everything else — search, key bindings,
command templates, options — works as in upstream.

```text
╭────────────────────────────────────────────────────────────────────────────╮
│                                                                            │
╰────────────────────────────────────────────────────────────────────────────╯
╭  devs (2)  │  blue (2)  │  green (2)  │  all (6)  ─────────────────────────╮
│   Name          Aliases        User           Destination    Port          │
│   DEV                                                                      │
│     dev-web                    deploy         10.0.0.10                    │
│     dev-db                     deploy         10.0.0.11                    │
│   ACME                                                                     │
│     acme-web-b                 deploy         10.1.0.10                    │
│     acme-web-g                 deploy         10.1.0.20                    │
│   ZETA                                                                     │
│     zeta-web-b                 deploy         10.2.0.10                    │
│     zeta-db-g                  deploy         10.2.0.21      2222          │
│                                                                            │
╰────────────────────────────────────────────────────────────────────────────╯
╭────────────────────────────────────────────────────────────────────────────╮
│(Esc) quit | (↑/↓) move | (←/→, tab) switch tab | (enter) select | (ctrl+r) │
╰────────────────────────────────────────────────────────────────────────────╯
```

## Features added in this fork

### Groups by client

Hosts are grouped by the part of their name before the first `-`, so `acme-web-b` and
`acme-web-g` land under an `ACME` header, and hosts are indented under it. A new client becomes a
new group on its own — there is no list of clients to maintain. Hosts whose name has no `-` are
shown without a header.

Groups appear in the order of their first host. With `--no-sort` that is the order of
`~/.ssh/config`, so the group you put first stays on top.

### Colours by environment

The segment after the last `-` decides the colour of the row:

| Host name | Colour |
|---|---|
| `…-b` | blue |
| `…-g` | green |
| `dev-…` | orange |
| anything else | default |

### Tabs

The host table has four tabs in its top border, each with the number of hosts it holds:

| Tab | Shows | Colour |
|---|---|---|
| `devs` | hosts whose name starts with `dev-` | orange |
| `blue` | hosts ending in `-b` | blue |
| `green` | hosts ending in `-g` | green |
| `all` | every host | yellow |

The app starts on `devs`. The search applies within the active tab and is kept when you switch.

## Naming convention

The features above read everything from the host name, following
`<client>-<role>-<slot>`:

```nginx
Host acme-web-b        # group ACME, blue
  HostName 10.1.0.10

Host acme-web-g        # group ACME, green
  HostName 10.1.0.20

Host dev-db            # group DEV, orange, "devs" tab
  HostName 10.0.0.11
```

## Keys

| Key | Action |
|---|---|
| `←` / `→`, `Shift+Tab` / `Tab` | previous / next tab (wraps around) |
| `↑` / `↓`, `Ctrl+P` / `Ctrl+N`, `Ctrl+K` / `Ctrl+J` | move the selection |
| `Home` / `End`, `PageUp` / `PageDown` | jump |
| `Enter` | connect to the selected host |
| `Ctrl+R` | reload the configuration |
| `Esc`, `Ctrl+C` | quit |
| any other key | type into the search |

`←` and `→` switch tabs, so they no longer move the cursor inside the search field.

## Building

Requires [Rust](https://www.rust-lang.org/) and Cargo ([rustup](https://rustup.rs)) and `ssh` on
your `PATH`.

```bash
git clone https://github.com/weriyard/power-sshs.git
cd power-sshs
cargo build --release
```

The binary is `./target/release/sshs`. Copy it to a directory on your `PATH`, for example
`~/.local/bin`.

To keep the group order of your config file, run it with `--no-sort`, or make that the default:

```bash
alias sshs='sshs --no-sort'
```

## Options

```text
-c, --config <CONFIG>...               SSH configuration files [default: /etc/ssh/ssh_config ~/.ssh/config]
    --show-proxy-command               Show ProxyCommand
-s, --search <SEARCH>                  Initial search filter
    --color <COLOR>                    Interface colour, a Tailwind palette name [default: blue]
    --sort / --no-sort                 Sort hosts by name (default) / keep the configuration file order
    --sort-fancy                       Sort search results by fuzzy match score
-t, --template <TEMPLATE>              Command to run, Handlebars [default: ssh "{{{name}}}"]
    --on-session-start-template <T>    Command to run when a session starts
    --on-session-end-template <T>      Command to run when a session ends
-e, --exit                             Exit after the SSH session ends
```

## Keeping up with upstream

```bash
git remote add upstream https://github.com/quantumsheep/sshs.git
git fetch upstream
git merge upstream/main
```

## Troubleshooting

### [...]/.ssh/config: no such file or directory

- Check if you have `~/.ssh/config` file
- If you don't, you can create it with `touch ~/.ssh/config`

If you want to use another SSH config file, you can use the `--config` option.

Here's a sample `~/.ssh/config` file:

```nginx
Host *
  AddKeysToAgent yes
  UseKeychain yes
  IdentityFile ~/.ssh/id_rsa

Host "My server"
  HostName server1.example.com
  User root
  Port 22

Host "Go through Proxy"
  HostName server2.example.com
  User someone
  Port 22
  ProxyCommand ssh -W %h:%p proxy.example.com
```

You can check the [OpenBSD `ssh_config` reference](https://man.openbsd.org/ssh_config.5) for more information on how to setup `~/.ssh/config`.

## License

MIT, as upstream — see [LICENSE](LICENSE). Original work © Nathanael Demacon.
