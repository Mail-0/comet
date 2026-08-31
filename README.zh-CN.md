# Zeron

在本地管理 Keiki 编码 agent，也可以使用内置的 Copilot 本地聊天。

*[English](README.md) | 简体中文*

每台设备各跑一个小引擎，会话就存在这台设备上。装完默认是纯本地模式，不用账号，也不用联网。

## 在本地安装运行（Linux）

Linux 发行版提供 tarball、`.deb` 软件包和 AppImage。你可以从 GitHub
Release 下载 `zeron-<version>-linux-<arch>.tar.gz`，解压后运行其中的安装脚本：

```bash
tar -xzf zeron-<version>-linux-<arch>.tar.gz
cd zeron-<version>-linux-<arch>
./install.sh
zeron status
```

也可以使用 `apt` 安装对应的 `.deb`，或直接运行 AppImage：

```bash
sudo apt install ./zeron-<version>-linux-<debarch>.deb
./zeron-<version>-linux-<arch>.AppImage
```

安装脚本会马上把守护进程拉起来，重启之后也会自己回来。不需要登录，也不需要配置同步。

日常命令：

```bash
zeron status      # 查看本地引擎状态
zeron daemon start|stop|restart|status
```

在桌面应用中登录 Keiki，即可管理组织中的 agent。聊天、会话和附件
都保存在本设备上；Comet 不再提供自己的账号或云同步层。

macOS 上用桌面版发行包，或者从源码构建 `zeron`，再运行 `zeron daemon install` 装上 launchd 服务。

---

想参与开发，或者好奇它怎么跑起来的？[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/zeronsh/comet)，也可以看 [ARCHITECTURE.md](ARCHITECTURE.md)。

采用 [MIT License](LICENSE)。
