# Synaps on Windows with WSL: getting started

This guide is for Windows 10 and 11 users who run Synaps inside WSL (Windows Subsystem for Linux). Inside WSL you run the Linux version of Synaps. Use these steps, not the Windows installer.

## Before you start

- WSL 2 with a Linux distribution, such as Ubuntu. To check, run `wsl -l -v` in PowerShell. The VERSION column must say 2.
- A WSL terminal. Windows Terminal works well.
- `curl`. If it is missing, install it: `sudo apt update && sudo apt install -y curl`

## 1. Install

In the WSL terminal:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/HaseebKhalid1507/SynapsCLI/releases/latest/download/synaps-installer.sh | sh
```

Open a new terminal, then check the install:

```bash
synaps --version
```

If it says `command not found`, add the install folder to your PATH:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
```

To update Synaps later, run the install command again.

## 2. Sign in

```bash
synaps login
```

Pick a provider: your subscription, an API key, a cloud account or a local model.

WSL usually has no browser of its own, so sign-in may not open one. Synaps then prints the sign-in address in the terminal.

1. Open that address in your Windows browser. In Windows Terminal, Ctrl+click it.
2. Sign in.
3. If the terminal says `Or paste the redirect URL here` and the browser shows an error page for `localhost`, copy the full address from the browser's address bar.
4. Paste it into the terminal and press Enter.

An API key needs no browser, for example `synaps login --provider kimi`.

Optional: to let sign-in open your Windows browser by itself, install these tools and set `BROWSER`:

```bash
sudo apt install -y xdg-utils wslu
echo 'export BROWSER=wslview' >> ~/.bashrc && source ~/.bashrc
```

## 3. Start

```bash
cd ~/my-project
synaps
```

- Keep your projects in your Linux home (`~/...`), not under `/mnt/c/...`. From WSL, files on the Windows drive are much slower.
- `/sessions` lists your sessions. `/model` changes the model. `/settings` opens the settings.

## How sessions work in WSL

- A background daemon owns your sessions. The first `synaps` starts it.
- When you close the last terminal, the daemon stops about 10 seconds later, unless a session is pinned (`/pin`). Nothing is lost. `synaps --continue` brings back your last session.
- `wsl --shutdown` (from Windows) also stops Synaps. Your sessions come back the same way.
- To check the daemon, run `synaps daemon status`. To stop it, run `synaps daemon stop`.

## Optional: turn on systemd

With systemd on, Synaps starts the daemon and each tool command in its own systemd scope. A runaway command that runs out of memory is then less likely to take the daemon and your other sessions down with it.

1. Open the WSL settings file: `sudo nano /etc/wsl.conf`
2. Add these lines and save:

   ```ini
   [boot]
   systemd=true
   ```

3. In PowerShell, run `wsl --shutdown`.
4. Open WSL again. Check that `systemctl --user status` answers.

## Good to know

- Synaps in WSL and Synaps installed on Windows are separate installs. Each has its own sign-in, settings and sessions.
- The `bash` tool runs in your WSL Linux shell, with your Linux tools.
- By default, WSL can't reach a model server on Windows (such as Ollama) at `localhost`. Run the model server inside WSL, or turn on WSL's mirrored networking.

## Troubleshooting

| Problem | Fix |
|---|---|
| `synaps: command not found` | Add `~/.cargo/bin` to your PATH (step 1). |
| No browser opens at sign-in | Open the printed address in your Windows browser (step 2). |
| Browser error page for `localhost` after sign-in | Paste the full address from the address bar into the terminal (step 2). |
| A project feels slow | Move it from `/mnt/c/...` to your Linux home (`~/...`). |
| `no daemon running` | `SYNAPS_DAEMON_AUTOSPAWN=0` is set. Unset it, or run `synaps daemon --detach`. |
