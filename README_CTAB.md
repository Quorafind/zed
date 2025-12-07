# CTab - Cursor Tab Completion for Zed

CTab 是一个为 Zed 编辑器实现的编辑预测提供器，兼容 Cursor AI 补全 API。

## 功能特性

### 核心功能

- **StreamCpp 补全** - 实现 Cursor 协议的流式代码补全
- **Multidiff 支持** - 支持多区域编辑的补全响应解析
- **Cursor Prediction** - 支持跨文件光标预测跳转
- **RecordCppFate** - 补全结果反馈（接受/拒绝）
- **CppAppend** - Followup 编辑追加支持

### 智能上下文引擎 (SmartContextEngine)

- **Import 分析** - 支持 Rust、TypeScript/JavaScript、Python、Go 等语言的导入语句解析
- **Recent Files 追踪** - MRU 文件列表，带时间衰减算法
- **LSP 集成** - 定义跳转、引用查找、悬停信息
- **语法索引** - 文件大纲、符号、声明追踪
- **异步预取** - 空闲时后台索引和 ripgrep 搜索
- **上下文缓存** - 30 秒 TTL 的评分上下文缓存

### 文件同步 (FileSyncManager)

- **增量同步** - 仅同步变更内容
- **指数退避重试** - 失败时自动重试
- **速率限制** - 基于服务器配置的请求限流
- **批量更新优化** - 合并多个更新请求

### 请求管理 (RequestStateManager)

- **DebounceManager** - 并发请求控制和去重（最多 6 个并发流）
- **SuggestionCache** - 缓存被取代的请求结果
- **NextActionManager** - 接受后自动触发下一次编辑
- **TriggerManager** - 智能触发机制，带拒绝冷却

### 差异追踪

- **DiffTracker** - 编辑历史和差异字符串生成
- **SnapshotDiffer** - 基于 BufferSnapshot 的精确差异提取

### 其他功能

- **LSP Warmup** - 打开文件时预热 LSP 定义缓存
- **Idle Trigger** - 空闲时自动触发补全
- **Request Logger** - 请求/响应日志记录（轮转文件）
- **Diagnostics Tracker** - 错误/警告收集

## 配置

在 Zed 的 `settings.json` 中添加：

```json
{
  "features": {
    "edit_prediction_provider": "ctab"
  },
  "ctab": {
    "auth_token": "your_auth_token",
    "base_url": "https://your-server.com",
    "endpoint_type": "selfhostedproxy"
  }
}
```

### 配置选项

| 选项 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `enabled` | boolean | 否 | `true` | 是否启用 CTab |
| `auth_token` | string | 是 | - | API 认证令牌 |
| `base_url` | string | 否 | `https://api2.cursor.sh` | API 服务器地址 |
| `client_key` | string | 否 | 自动生成 | 客户端校验密钥 |
| `endpoint_type` | string | 否 | `official` | 端点类型 |
| `model` | string | 否 | `auto` | 指定使用的模型 |
| `debounce_ms` | number | 否 | `75` | 防抖延迟（毫秒） |
| `max_completion_length` | number | 否 | `2000` | 最大补全长度 |
| `idle_trigger_ms` | number | 否 | `1500` | 空闲触发延迟（毫秒），0 为禁用 |

### 端点类型 (endpoint_type)

| 值 | 说明 |
|----|------|
| `official` | 官方 Cursor API (`api2.cursor.sh`)，使用 Connect RPC 格式 |
| `selfhostedproxy` | 代理服务器，转发请求到官方 API（使用官方路径，自定义 base_url） |
| `selfhosted` | 自托管服务器，使用简化的 API 路径（如 cursor-api 项目） |

### 配置示例

**官方 Cursor API：**
```json
{
  "features": {
    "edit_prediction_provider": "ctab"
  },
  "ctab": {
    "auth_token": "your_cursor_token",
    "endpoint_type": "official"
  }
}
```

**自托管代理：**
```json
{
  "features": {
    "edit_prediction_provider": "ctab"
  },
  "ctab": {
    "auth_token": "sk-xxxxxxxxxxxxxxxx",
    "base_url": "http://localhost:8080",
    "endpoint_type": "selfhostedproxy"
  }
}
```

**自托管服务器 (cursor-api)：**
```json
{
  "features": {
    "edit_prediction_provider": "ctab"
  },
  "ctab": {
    "auth_token": "your_token",
    "base_url": "http://localhost:8000",
    "endpoint_type": "selfhosted"
  }
}
```

## 协议支持

CTab 实现了 Cursor AI 的 gRPC-Web/Connect RPC 协议：

- `aiserver.v1.AiService/StreamCpp` - 流式补全
- `aiserver.v1.AiService/RecordCppFate` - 结果反馈
- `aiserver.v1.AiService/CppAppend` - Followup 追加
- `aiserver.v1.AiService/CppConfig` - 服务器配置
- `aiserver.v1.FileSyncService/FSUploadFile` - 文件上传
- `aiserver.v1.FileSyncService/FSSyncFile` - 文件同步

## 致谢

本项目的实现参考了以下开源项目：

- [Haleclipse/Cometix-Tab](https://github.com/Haleclipse/Cometix-Tab) - 提供了基本的实现思路
- [Cometix-Org/cometix-tab-copilot-exp](https://github.com/Cometix-Org/cometix-tab-copilot-exp) - 提供了 Flush Edit 相关的实现参考
- [wisdgod/cursor-tab](https://github.com/wisdgod/cursor-tab) - Cursor 协议定义 Proto 文件结构

## License

GPL-3.0-or-later
