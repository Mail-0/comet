# Zeron

Control your Keiki coding agents locally, with Copilot available as a built-in local chat.

*English | [简体中文](README.zh-CN.md)*

![Zeron driving a Claude Code session with a live branch diff sidebar](apps/landing/public/assets/app-screenshot.jpg)

Every device runs a small engine that stores sessions on that device. A new installation starts in local-only mode without an account or a network connection.

## Install and run locally (Linux)

```bash
curl -fsSL https://zeron.sh/install.sh | sh
zeron status
```

The installer starts the daemon immediately and keeps it running across reboots. No sign-in or sync configuration is required.

Day-to-day:

```bash
zeron status      # local engine status
zeron daemon start|stop|restart|status
```

Sign in to Keiki from the desktop app to manage your organization agents. Chats,
sessions, and their attachments remain on this device; Comet does not provide
an account or cloud synchronization layer.

On macOS: use the desktop release, or build `zeron` from source and run `zeron daemon install` to install the launchd service.

---

Developing or curious how it works? [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/zeronsh/comet) or check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
