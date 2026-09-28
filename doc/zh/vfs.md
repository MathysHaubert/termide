# 远程文件系统（VFS）

TermIDE 内置纯 Rust 的 VFS 层，让[文件管理器](file-manager.md)能够像
浏览本地目录一样浏览远程服务器。文件可以在编辑器中打开、在本地和远程
面板之间复制、重命名等——使用相同的界面，进度显示在[操作面板](operations.md)
中。

无需任何原生库：SFTP 基于 `russh` + `russh-sftp`，FTPS 基于 `rustls`。
在 Alpine / musl 上开箱即用地支持静态构建。

## 支持的协议

| 协议        | 说明                                               |
|-------------|----------------------------------------------------|
| `sftp://`   | 基于 SSH 的 SFTP                                   |
| `ftp://`    | 普通 FTP                                           |
| `ftps://`   | TLS 加密的 FTP（rustls）                           |

`smb://` 与 `nfs://` 会被 URL 解析器识别，但目前未提供对应的 provider
——请通过操作系统原生挂载使用它们。

## URL 语法

VFS URL 的形式为：

```
scheme://[user[:password]@]host[:port][/path]
```

示例：

```
sftp://nvn@example.com/home/nvn/projects
sftp://example.com:2222/srv/builds
ftp://files.example.com/pub
ftps://secure.example.com/uploads
```

说明：
- `path` 支持非 ASCII 字符；解析器在与服务器通信前会将其
  percent-decode 回 UTF-8。
- 省略用户名时 SFTP provider 会回退到 SSH 配置（见认证一节）。
- 省略端口时使用协议默认值（22 / 21 / 990）。
- URL 中嵌入密码可用但不推荐——SFTP 优先使用基于密钥的认证。

## 打开远程位置

打开远程面板的两种方式：

1. **Go to path** — 在文件管理器的 `Go to path` 输入框中粘贴 / 输入
   VFS URL。
2. **Bookmarks** — 把常用的远程位置保存一次，然后通过 `Bookmarks`
   菜单访问。书签条目接受同样的 URL 语法，保存在
   `~/.config/termide/bookmarks.toml`（或项目本地的
   `.termide/bookmarks.toml`，用于团队共享）。

两种方式都会打开一个根目录指向远程目录的普通文件管理器视图；其他
所有操作（树展开、复制、重命名、按 `Space` 看属性）与本地面板完全
一致。

## 认证（SFTP）

SFTP 支持四种认证方式：

- **`Auto`**（默认）— 先尝试 SSH agent，然后是 `~/.ssh/config` 中针对
  该主机列出的密钥（包括 `IdentityFile`、`User`、`Port` 和 `Hostname`
  别名），再尝试默认密钥（`id_ed25519`、`id_rsa`、`id_ecdsa`、
  `id_dsa`），最后是密码。
- **SSH agent** — 当 `SSH_AUTH_SOCK` 指向正在运行的 agent 时使用。
- **SSH key** — 明确指定私钥文件，可选带密码短语。
- **Password** — 交互输入或预先存储的值。

由于 `Auto` 会读取 `~/.ssh/config`，可以在书签里只写简短 URL
（`sftp://my-build-host/path`），让 SSH 配置补全真实的主机名、用户
和密钥文件——这与 `ssh` CLI 使用的是同一份配置。

## 远程面板的体验

所有远程操作都是异步的：

- 首次目录列表显示短暂的旋转图标；条目在服务器返回时出现。
- 展开子目录会插入小的 `…` 占位符；列表到达后会被真实子项替换。
- 重新加载时保留树状态、光标和选择。

远程面板**不**计算 wide view 的目录大小——为一列数据而走完整个远程
树的代价远超收益。远程目录的 Size 列始终为空。

## 文件传输

在一个面板里选中文件，按 `C` / `F5`（复制）或 `M` / `F6`（移动），
选择目标——本地路径或另一个远程面板。传输会作为操作注册到
[操作面板](operations.md)中，包含：

- 真实的进度条（字节 + 文件），worker 在每个 chunk 后更新。
- 可真正暂停字节流的 Pause / Resume（被暂停的上传 / 下载停在
  worker 这一侧；SFTP actor 仍可服务其他面板的 metadata 请求）。
- 在 chunk 之间干净地停止的 Cancel。如果取消操作在服务器上留下了
  部分文件，面板会询问是否删除——参见[操作面板](operations.md)的
  取消清理章节。

同一 SFTP / FTP 连接内的重命名留在服务器端：单一连接内的移动以
remote-rename 完成，而不是 download-then-upload。

## 项目

含远程文件管理器面板的项目布局像本地路径一样保存 URL。下次启动时
TermIDE 在后台重新连接，并以加载占位符显示面板，直到列表到达——
UI 的其余部分立即可响应。

### 连接断开

若远程会话丢失（空闲超时、网络中断），下一次操作会失败，面板弹出恢复
对话框，提供三个选项：**重新连接**（对同一路径开启新会话）、**打开主目录
（本地）**（断开连接并将面板切换到本地主目录）或**关闭面板**。按 `Esc`
关闭对话框并保留面板的最后列表。不再出现错误的无限循环。

## 限制 / 已知缺口

- 目前没有 `smb://` / `nfs://` provider，只有 URL 解析。
- 中断的传输没有按 byte offset 续传：取消后的上传重新发起时从头
  开始。
- TermIDE 内部无法持久保存交互密码;长期使用请配置 SSH 密钥或
  依赖 SSH agent。
