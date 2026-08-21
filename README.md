# TinyImage 2.1.1

纯本地桌面图片压缩工具（**Tauri 2 + Rust**）。  
不调用 TinyPNG，不消耗云端额度，断网可压缩。

支持：JPG / PNG / WebP / GIF / BMP / TIFF / ICO。  
主界面可打开 **打赏支持**（本地展示支付宝 / 微信收款码，不联网）。

## 开发

在项目根目录：

```bash
npm install
npm run start
```

**Jpegli 构建**需本机 **CMake** + **MSVC Build Tools**（首次 `cargo`/`tauri` 会编译 `src-tauri/vendor/jpegli-sys`）。

**不要**在未授权时执行打包命令。

## 测试

```bash
npm run test:rust
```

## 打包（须授权）

仅当维护者书面 **「授权打包」**（或「授权 release」）后执行：

```bash
npm run package:win
```

产物：`src-tauri\target\release\bundle\nsis\TinyImage_2.1.1_x64-setup.exe`

本阶段**无**自动更新、**无**代码签名。

## 仓库结构（精简）

| 路径 | 职责 |
|---|---|
| `src/` | 前端（Vite + TypeScript） |
| `src-tauri/` | Rust 压缩引擎、队列、Tauri 壳 |
| `legacy-electron/` | 旧 Electron 1.2.x（仅对照，不演进） |
| `身份卡_优化版.md` | AI / 协作纪律 |
| `技术实施白皮书_v1.0.md` | 产品蓝图（文内修订号为准） |

## 文档

- `身份卡_优化版.md`
- `技术实施白皮书_v1.0.md`（当前 **v1.30.0** · T032 打赏；T004 已结案）
