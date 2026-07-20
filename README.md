# bili_add_on

为视频叠加 B 站弹幕（danmaku）的命令行工具。支持从 B 站视频 ID 自动获取弹幕 XML，也支持指定本地弹幕 XML 文件。

## 依赖

- [ffmpeg](https://ffmpeg.org/) — 视频编解码必须，请确保已安装并在 PATH 中可用

## 安装

```bash
cargo build --release
```

编译产物位于 `target/release/bili_add_on`（Windows 下为 `bili_add_on.exe`）。

## 用法

### 基本用法

```bash
# 从 B 站视频 ID 拉取弹幕并叠加到本地视频
bili_add_on --input input.mp4 --bvid BV1xxxxxxxxxx

# 使用本地弹幕 XML 文件
bili_add_on --input input.mp4 --xml danmaku.xml
```

`--bvid` 和 `--xml` 二者必选其一，不可同时使用。

### 参数说明

| 参数 | 简写 | 默认值 | 说明 |
|------|------|--------|------|
| `--input` | `-i` | **必填** | 输入视频文件路径 |
| `--output` | `-o` | `bili_add_on_<源文件名>` | 输出视频路径 |
| `--bvid` | | | B 站视频 ID（如 `BV1xxxxxxxxxx`），自动拉取弹幕 |
| `--xml` | `-x` | | 本地弹幕 XML 文件路径 |
| `--opacity` | | `0.5` | 弹幕不透明度，取值范围 0~1 |
| `--top-ratio` | `-t` | `0.0` | 弹幕显示区域上界与画面高度比值，0 为顶端 |
| `--bottom-ratio` | `-b` | `1.0` | 弹幕显示区域下界与画面高度比值，1 为底端 |
| `--font-scale` | | `1.0` | 弹幕字号缩放比 |
| `--speed` | `-s` | `3` | 弹幕滚动速度（像素每帧） |
| `--line-spacing` | | `4` | 弹幕行间距（像素） |
| `--fixed-duration` | | `5.0` | 固定弹幕（顶部/底部）的持续时间（秒） |

### 弹幕区域控制

```
┌─────────────────────┐  ← 上限 (top-ratio=0.0)
│                     │
│    弹幕显示区域       │
│                     │
└─────────────────────┘  ← 下限 (bottom-ratio=1.0)
```

通过调整 `--top-ratio` 和 `--bottom-ratio` 可以控制弹幕在画面中的垂直显示范围。例如将弹幕限制在视频上半部分：

```bash
bili_add_on --input input.mp4 --bvid BV1xxxxxxxxxx --top-ratio 0.0 --bottom-ratio 0.5
```

## 支持的弹幕模式

| 模式 | ID | 说明 |
|------|----|------|
| 滚动弹幕 | 1/2/3 | 从右向左滚动显示 |
| 底端弹幕 | 4 | 固定在画面底部 |
| 顶端弹幕 | 5 | 固定在画面顶部 |
| 逆向弹幕 | 6 | 从左向右滚动显示 |
| BAS 弹幕 | 9 | 特殊底部弹幕 |

高级弹幕（mode 7）当前仅解析，不参与渲染。

## 输出格式

- 视频编码：H.264 (YUV420p)
- 输出文件默认命名规则：`bili_add_on_<源文件名>`

## 许可证

MIT
