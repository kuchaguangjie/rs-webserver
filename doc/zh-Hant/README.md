**語言 / Language:** [English](../../README.md) | [簡體中文](../zh-Hans/README.md) | 繁體中文

# rs-webserver

一個以 Rust 標準函式庫實作的極簡靜態檔案 HTTP 伺服器，具備**固定大小執行緒池 + 有界任務佇列**。
這是 *The Rust Book* 結尾專案（單執行緒伺服器 → 多執行緒伺服器）的整理版本，
並在此基礎上補充了**設定檔**、**背壓（503）**、**詳細註解**、**單元測試** 與 **Makefile**。

## 特色

- 純標準函式庫，**零第三方相依**；
- 生產者-消費者 / 工作佇列模型：`main` 把任務推入佇列，閒置 worker 自行拉取執行；
- 任務佇列**有界**（`max_queue_size`，預設 10000），佇列滿時回傳 **503** 形成背壓，
  避免任務無限堆積導致記憶體暴增；
- 透過 `config.yml` 設定執行緒池大小、佇列容量、資源目錄、監聽位址；
- 工作執行緒內不 panic，避免單一壞請求「殺死」執行緒導致池容量下降。

## 目錄結構

```
rs-webserver/
├── Cargo.toml           # 套件定義（無外部相依）
├── LICENSE              # Apache License 2.0 全文
├── config.yml           # 執行時設定
├── Makefile             # 常用指令封裝
├── README.md            # 英文（預設文件）
├── doc/
│   ├── zh-Hans/
│   │   └── README.md    # 簡體中文
│   └── zh-Hant/
│       └── README.md    # 繁體中文（本文件）
├── resource/
│   └── html/            # 靜態資源
│       ├── hello.html   # GET / 回傳
│       └── 404.html     # 未匹配路徑回傳
└── src/
    ├── main.rs          # 二進位入口：監聽、路由、背壓、回寫回應
    ├── lib.rs           # ThreadPool（有界佇列執行緒池）
    └── config.rs        # 設定載入（YAML 子集解析器）
```

## 建置與執行

需要 Rust 1.85+（本專案使用 edition 2024；開發環境為 1.98）。

```bash
# 方式一：直接使用 cargo
cargo run                     # 讀取目前目錄下的 config.yml
cargo run -- path/to/conf.yml # 指定設定檔

# 方式二：使用 Makefile
make run
make smoke     # 啟動後自動對各路由做一次冒煙測試
make test      # 執行單元測試
make clippy    # 靜態檢查
```

瀏覽器或 curl 存取：

```bash
curl http://127.0.0.1:7878/       # 200 -> hello.html
curl http://127.0.0.1:7878/sleep  # 5 秒後 200 -> hello.html（用於示範阻塞）
curl http://127.0.0.1:7878/nope   # 404 -> 404.html
```

## 設定（config.yml）

| 設定項 | 說明 | 預設值 |
| --- | --- | --- |
| `pool_size` | 執行緒池工作執行緒數（正整數） | `4` |
| `max_queue_size` | 任務佇列容量上限（正整數） | `10000` |
| `resources_dir` | 靜態資源目錄 | `resource/html` |
| `bind_address` | 監聽位址 | `127.0.0.1:7878` |

- 所有項均可省略，省略時使用預設值；**找不到設定檔時也回退到預設值**。
- 未知設定項會報錯結束，便於發現拼字錯誤。
- 解析器只支援扁平的 `key: value`（含註解與行尾註解、帶引號的值），
  詳見 `src/config.rs` 頂部文件。

範例：

```yaml
pool_size: 8
max_queue_size: 10000
resources_dir: resource/html
bind_address: 0.0.0.0:8080
```

---

## 運作原理

### 整體模型：生產者-消費者 / 工作佇列

本專案**不是**非同步模型，而是經典的**執行緒池 + 共享任務佇列**（也稱工作佇列 / pull 模型）：

```
                     execute(job)                Arc<Mutex<Receiver>>
  生產者(main) ───────────────────> [ 有界任務佇列 ] <──────────────────┐
  (accept 迴圈)                      (容量=max_queue_size)   ▲   ▲   ▲   ▲
                                                             │   │   │   │
                                                          Worker0 ... WorkerN
```

- 它是**行程內、記憶體中**的單條佇列，**不是** broker 式 MQ（沒有持久化、跨行程、確認/重投）；
- 多個 `execute` 呼叫 = **多生產者**，多個 worker 共享接收端 = **多消費者**，
  底層用 std 的 *MPSC* 通道 + `Arc<Mutex<Receiver>>` 組出 MPMC 的效果；
- 任務與執行緒之間**沒有固定綁定**：誰先閒置，誰就取走下一個任務。

### 一個連線的處理流程

1. `main` 在 `listener.incoming()` 上阻塞等待新連線；
2. 拿到連線後 `try_clone()` 出一份控制代碼（原件留作「佇列滿時回 503」的後備），
   把控制代碼和資源路徑 `Arc::clone` 一份，建構任務閉包；
3. 呼叫 `ThreadPool::execute` 把閉包**推入**佇列（`Box<dyn FnOnce()>`）：
   - 入列成功 → 回傳 `Ok(())`；
   - 佇列已滿 → 回傳 `Err(QueueFull)`，`main` 立刻給用戶端回 **503** 並繼續服務下一個連線；
4. 某個閒置 worker 的 `recv()` 拿到任務，執行 `handle_connection`；
5. `handle_connection` 解析請求行 → 選資源 → 讀檔案 → 回寫回應。

### 為什麼是 `Arc<Mutex<Receiver>>`

- `Receiver` 不能被 `clone`，但要讓 N 個 worker 共享同一個接收端，所以 `Arc` 包一層；
- `recv()` 需要 `&mut self`，而多個 worker 不能同時可變借用，所以 `Mutex` 提供內部可變性。

關鍵細節：鎖**只在 `recv()` 期間持有**——

```rust
let message = receiver.lock().unwrap().recv(); // 臨時守衛在本敘述句末尾即釋放
// 之後才執行任務，此時不持鎖
```

因此同一時刻只有一個 worker 阻塞在 `recv()` 上（不會出現驚群/序列化），
而多個 worker 可以**並行執行**各自的任務，鎖不會成為吞吐瓶頸。

### 有界佇列與背壓

佇列由 `mpsc::sync_channel(max_queue_size)` 建立（**有界**），提交用非阻塞的 `try_send`：

- 系統**在途任務上限 = `pool_size` + `max_queue_size`**
  （`pool_size` 個正在執行 + `max_queue_size` 個排隊）；
- 達到上限後新任務被立即拒絕 → HTTP 層回傳 `503 Service Unavailable`；
- 之所以用 `try_send` 而不是阻塞式 `send`：`main` 是單執行緒 accept 迴圈，
  一旦阻塞就會連帶停止接收新連線；回傳錯誤則允許它「拒絕這一個、繼續服務其它」。

### 優雅關閉

`ThreadPool` 被 drop 時：先丟棄發送端 → 佇列關閉 → 各 worker 的 `recv()` 回傳 `Err`、
結束迴圈 → 主執行緒逐個 `join`，確保執行緒與任務不洩漏。

---

## 常見問題（設計問答）

### Q1：執行緒池的執行緒全部被佔用時會發生什麼？

差別在於佇列是否有界：

- **本專案的做法（有界佇列）**：任務先排隊；當排隊數達到 `max_queue_size`
  且沒有閒置執行緒時，新任務**被立即拒絕**，HTTP 層回傳 **503**。
  系統在途任務數有硬上限（`pool_size + max_queue_size`），記憶體不會無界成長。
- 若用**無界佇列**（`mpsc::channel()`）：任務永遠不會被拒，只會一直堆積，
  表現為**延遲線性成長 + 記憶體持續成長**，生產快於消費時最終可能 OOM。

所以「池子用滿」不應默默堆積，而應透過**背壓**把壓力回饋給上游。

### Q2：如果第 3 個執行緒卡住了，會影響第 4 個執行緒嗎？為什麼？

**不會直接影響第 4 個執行緒本身。** 原因在於 worker 與請求之間**沒有固定綁定關係**：
所有 worker 從同一個共享佇列裡「搶」任務，任務交給誰是不確定的。

- 第 3 個 worker 卡住時，第 4 個 worker 仍會正常加鎖、`recv()`、執行其它任務，
  兩者之間沒有鎖相依（鎖只在 `recv()` 期間短暫持有，執行任務時不持鎖，
  所以不會互相阻塞）；
- 真正的影響是**並行容量減少一個**：可用 worker 從 N 變成 N-1，吞吐下降，
  佇列更容易堆積（進而更容易觸發 503）。

需要區分的兩種後果：

1. **部分執行緒卡住**：其它執行緒照常運作，只是整體吞吐降低；
2. **所有執行緒都卡住**：沒有任何 worker 有空去 `recv()` 佇列裡的新任務，
   於是**哪怕是 `GET /` 這種極快的請求也會被排在慢請求後面**——
   這就是典型的**隊頭阻塞（head-of-line blocking）**，表現為整個服務「假死」。

因此，個別執行緒卡住不會「連坐」特定執行緒，但只要卡住的執行緒足夠多（尤其佔滿全部），
就會拖垮整個服務。緩解手段：**請求逾時**、**隔離**（把慢操作放到獨立的池/佇列）、
**有界佇列 + 背壓**（本專案已具備），或改用**非同步 I/O**（tokio 等）。

### 實測資料（本倉庫實際跑出來的）

- **背壓**：`pool_size=1, max_queue_size=2`，並行發起 6 個 `/sleep`（各佔執行緒 5s）——
  容量 = 1 執行 + 2 排隊 = 3，結果恰好 **3 個回傳 200、3 個回傳 503**，隨後服務恢復正常。
- **隊頭阻塞**：`pool_size=4`，並行發起 4 個 `/sleep` 佔滿全部執行緒後，再發一個 `GET /`，
  這個快請求等了 **4.49s** 才被服務（`time_total≈4.491537s`）——
  說明執行緒被慢請求佔滿後，快請求同樣要排隊。

## 授權條款

本專案基於 **[Apache License 2.0](../../LICENSE)** 授權，完整條文見 [`LICENSE`](../../LICENSE)。

如需在原始碼檔案開頭加上著作權聲明，可使用 Apache 官方推薦的範本：

```text
Copyright 2025 eric

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

除非適用法律要求或書面同意，本授權條款下散佈的軟體按「原樣」提供，
不附帶任何明示或暗示的擔保或條件。
