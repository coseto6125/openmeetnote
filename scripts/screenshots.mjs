/**
 * 產生 README 用的介面截圖。
 *
 * 走的是真正的前端程式碼與樣式：示範資料從 `window.__TAURI_INTERNALS__`
 * 灌進去，也就是 Tauri 自己的 IPC 入口，因此每個畫面都是元件對真實資料形狀
 * 的實際渲染，不是另外做的一張圖。介面一改，重跑這支腳本截圖就跟著更新。
 *
 * 內容是編出來的示範會議。README 是公開的，而真實會議紀錄不該因為要放張圖
 * 就變成公開資料。
 *
 *   pnpm exec node scripts/screenshots.mjs
 */
import { spawn } from 'node:child_process';
import { mkdir } from 'node:fs/promises';
import { chromium } from 'playwright';

const PORT = 5199;
const OUT = 'docs/images';
/** 跟 tauri.conf.json 的視窗尺寸一致，截出來的比例才是使用者會看到的比例 */
const VIEWPORT = { width: 1320, height: 860 };

/* ── 示範會議 ─────────────────────────────────────────────────── */

const SPEAKERS = [
  { speakerId: 's1', ordinal: 1, proposedName: null, confirmedName: '我', track: 'mic', mergedInto: null },
  { speakerId: 's2', ordinal: 2, proposedName: '沈立群', confirmedName: '沈立群', track: 'system', mergedInto: null },
  { speakerId: 's3', ordinal: 3, proposedName: null, confirmedName: null, track: 'system', mergedInto: null },
];

const LINES = [
  ['s2', '離線那一版的同步先講一下，上禮拜壓測跑出來的數字不太好看。', 12_400],
  ['s2', '兩萬筆變更，全量比對要四十七秒，使用者會以為當掉了。', 19_800],
  ['s1', '四十七秒是在哪個裝置上量的？', 27_100],
  ['s2', '是我這台，M2 Pro。舊機器只會更慢。', 30_600],
  ['s3', '那個比對是每次都掃全表嗎，還是有帶版本號？', 38_200],
  ['s2', '目前是全表。版本號欄位有，但同步邏輯沒有用到它。', 43_900],
  ['s1', '所以改成增量的話，理論上會降到只掃有變動的那些。', 52_300],
  ['s2', '對，估下來是兩到三秒。但要處理衝突，現在是後寫覆蓋前寫。', 57_800],
  ['s3', '後寫覆蓋在離線情境下會掉資料，兩台裝置各改各的就沒了。', 68_400],
  ['s1', '那這件事要先決定：是接受掉資料，還是做衝突合併。', 76_200],
  ['s2', '我傾向合併，但那是一個 sprint 的量，不是這週能收掉的。', 83_500],
  ['s3', '先做增量、衝突先擋住不讓它靜默覆蓋，可以嗎？', 92_100],
  ['s1', '可以。擋住的意思是跳出來讓使用者選，還是整筆退回？', 98_700],
  ['s3', '讓使用者選。退回的話他不知道自己剛剛白做了。', 104_300],
];

const NOTES = [
  { noteId: 1, text: '壓測環境：M2 Pro / 20k 筆變更 / 全量比對 47 秒', meetingTimeMs: 22_000, capturedAudioMs: 22_000 },
  { noteId: 2, text: '衝突合併估一個 sprint，這次不做，但要擋住靜默覆蓋', meetingTimeMs: 88_000, capturedAudioMs: 88_000 },
];

const segments = LINES.map(([speakerId, text, ms], i) => ({
  segmentId: i + 1,
  revision: 1,
  origin: 'provider',
  speakerId,
  text,
  meetingTimeMs: ms,
}));

/** LiveView 吃的是事件流，所以照真的順序送一批進去 */
const events = [
  { kind: 'meetingStateChanged', seq: 1, state: 'recording', meetingTimeMs: 0, capturedAudioMs: 0 },
  ...SPEAKERS.map((s, i) => ({
    kind: 'speakerProposed',
    seq: 2 + i,
    speakerId: s.speakerId,
    ordinal: s.ordinal,
    proposedName: s.proposedName,
    track: s.track,
  })),
  ...SPEAKERS.filter((s) => s.confirmedName).map((s, i) => ({
    kind: 'speakerConfirmed',
    seq: 5 + i,
    speakerId: s.speakerId,
    name: s.confirmedName,
  })),
  ...segments.map((s, i) => ({
    kind: 'transcriptFinalized',
    seq: 7 + i,
    segmentId: s.segmentId,
    revision: 1,
    origin: 'provider',
    speakerId: s.speakerId,
    text: s.text,
    meetingTimeMs: s.meetingTimeMs,
    capturedAudioMs: s.meetingTimeMs,
  })),
  ...NOTES.map((n, i) => ({
    kind: 'noteAdded',
    seq: 21 + i,
    noteId: n.noteId,
    text: n.text,
    meetingTimeMs: n.meetingTimeMs,
    capturedAudioMs: n.capturedAudioMs,
  })),
  {
    kind: 'snapshotCreated',
    seq: 23,
    version: 1,
    throughEventSeq: 22,
    meetingTimeMs: 106_000,
    prompt: '整理決議與待辦',
  },
  { kind: 'generationCompleted', seq: 24, version: 1 },
  {
    kind: 'trackActivity',
    micLevel: 0.34,
    systemLevel: 0.61,
    micHealth: 'ok',
    systemHealth: 'ok',
    sttHealth: 'ok',
  },
];

const BATCH = {
  firstSeq: 1,
  lastSeq: 24,
  prevHighSeq: 0,
  emittedAtMs: Date.now(),
  meetingTimeMs: 109_000,
  capturedAudioMs: 109_000,
  state: 'recording',
  journalError: null,
  events,
};

/** 結束會議。歷史與成果那幾張截的是一場開完的會，頂欄不該還寫著「錄音中」。 */
const STOPPED = {
  firstSeq: 25,
  lastSeq: 25,
  prevHighSeq: 24,
  emittedAtMs: Date.now(),
  meetingTimeMs: 3_120_000,
  capturedAudioMs: 3_120_000,
  state: 'completed',
  journalError: null,
  events: [
    {
      kind: 'meetingStateChanged',
      seq: 25,
      state: 'completed',
      meetingTimeMs: 3_120_000,
      capturedAudioMs: 3_120_000,
    },
  ],
};

const ref = (segmentId, quotedText) => ({
  sourceKind: 'transcript_segment',
  sourceId: String(segmentId),
  sourceRevision: 1,
  locator: `seg:${segmentId}`,
  quotedText,
  quotedTextSha256: 'a'.repeat(64),
  validationStatus: 'valid',
});

const block = (position, kind, claimKind, content, sourceRefs = []) => ({
  position,
  kind,
  claimKind,
  content: JSON.stringify(content),
  sourceRefs,
});

const DOCUMENT = [
  block(1, 'callout', 'inference', {
    type: 'callout',
    tone: 'summary',
    title: '成果摘要',
    body: '離線同步的全量比對在兩萬筆變更時要四十七秒，本次決定改為依版本號的增量比對。衝突處理維持這次不做合併，但不再允許靜默的後寫覆蓋，改為讓使用者選擇。',
  }),
  block(2, 'heading', 'fact', { type: 'heading', level: 2, text: '問題與量測' }),
  block(
    3,
    'paragraph',
    'fact',
    {
      type: 'text',
      text: '目前的同步在每次執行時掃描全表，不使用既有的版本號欄位。壓測在 M2 Pro 上以兩萬筆變更量得四十七秒，較舊的機器只會更慢。',
    },
    [ref(2, '兩萬筆變更，全量比對要四十七秒'), ref(6, '目前是全表。版本號欄位有，但同步邏輯沒有用到它。')],
  ),
  block(
    4,
    'table',
    'fact',
    {
      type: 'table',
      headers: ['作法', '兩萬筆的耗時', '資料風險'],
      rows: [
        ['全量比對（現況）', '47 秒', '後寫覆蓋前寫，離線編輯會靜默消失'],
        ['依版本號增量', '2 到 3 秒（估）', '同上，除非同時處理衝突'],
        ['增量 + 衝突合併', '2 到 3 秒（估）', '無，但工作量約一個 sprint'],
      ],
    },
    [ref(8, '估下來是兩到三秒')],
  ),
  block(
    5,
    'decision',
    'fact',
    { type: 'text', text: '同步改為依版本號的增量比對，衝突合併這次不做。' },
    [ref(12, '先做增量、衝突先擋住不讓它靜默覆蓋')],
  ),
  block(
    6,
    'decision',
    'fact',
    { type: 'text', text: '偵測到衝突時跳出讓使用者選擇，不整筆退回。' },
    [ref(14, '讓使用者選。退回的話他不知道自己剛剛白做了。')],
  ),
  block(7, 'actionItem', 'fact', {
    type: 'actionItem',
    text: '把版本號接進同步邏輯，補上兩萬筆的迴歸壓測',
    owner: '沈立群',
    due: null,
  }),
  block(8, 'actionItem', 'fact', {
    type: 'actionItem',
    text: '設計衝突選擇的介面，含「兩邊都留」的情況',
    owner: null,
    due: null,
  }),
  block(9, 'gap', 'gap', {
    type: 'text',
    text: '沒有討論到三台以上裝置同時離線編輯的情況，會上只提到兩台。',
  }),
  block(10, 'suggestion', 'suggestion', {
    type: 'text',
    text: '衝突合併排進下個 sprint 之前，先量一次實際的衝突發生率。若極低，選擇介面可能就足夠。',
  }),
];

const MEETING = {
  id: 7,
  title: '離線同步機制設計討論',
  state: 'completed',
  startedAt: '2026-07-28T14:00:00+08:00',
  endedAt: '2026-07-28T14:52:00+08:00',
  meetingTimeMs: 3_120_000,
  capturedAudioMs: 3_120_000,
  highSeq: 24,
  segmentCount: 14,
  noteCount: 2,
  documentCount: 1,
};

const OTHER_MEETINGS = [
  { ...MEETING },
  {
    id: 6,
    title: '轉錄品質回歸檢討',
    state: 'completed',
    startedAt: '2026-07-24T10:30:00+08:00',
    endedAt: '2026-07-24T11:12:00+08:00',
    meetingTimeMs: 2_520_000,
    capturedAudioMs: 2_520_000,
    highSeq: 412,
    segmentCount: 186,
    noteCount: 5,
    documentCount: 2,
  },
  {
    id: 5,
    title: '第三季路線圖',
    state: 'completed',
    startedAt: '2026-07-21T09:00:00+08:00',
    endedAt: '2026-07-21T10:34:00+08:00',
    meetingTimeMs: 5_640_000,
    capturedAudioMs: 5_640_000,
    highSeq: 803,
    segmentCount: 341,
    noteCount: 11,
    documentCount: 3,
  },
];

const RUNS = [
  {
    runId: 11,
    documentId: 7,
    versionNo: 1,
    throughEventSeq: 22,
    status: 'completed',
    title: '離線同步機制設計討論',
    purpose: '整理決議與待辦',
    prompt: '整理決議與待辦',
    failureReason: null,
    createdAt: '2026-07-28T14:53:10+08:00',
  },
];

const DETAIL = {
  summary: MEETING,
  segments: segments.map((s) => ({
    segmentId: s.segmentId,
    revision: 1,
    origin: 'provider',
    speakerId: s.speakerId,
    text: s.text,
    track: s.speakerId === 's1' ? 'mic' : 'system',
    meetingStartMs: s.meetingTimeMs,
    meetingEndMs: s.meetingTimeMs + 5_000,
    userEdited: false,
  })),
  notes: NOTES,
  speakers: SPEAKERS.map((s) => ({
    speakerId: s.speakerId,
    ordinal: s.ordinal,
    proposedName: s.proposedName,
    confirmedName: s.confirmedName,
    status: s.confirmedName ? 'confirmed' : 'default',
  })),
  runs: RUNS,
};

const SETTINGS = [
  {
    kind: 'stt',
    provider: { value: 'local-whisper', source: 'default' },
    model: { value: 'ggml-large-v3-turbo-q5_0', source: 'settings' },
    baseUrl: { value: '', source: 'default' },
    secret: 'missing',
  },
  {
    kind: 'llm',
    provider: { value: 'claude-code', source: 'settings' },
    model: { value: 'claude-opus-5', source: 'default' },
    baseUrl: { value: '', source: 'default' },
    secret: 'missing',
  },
];

const BACKENDS = [
  { id: 'claude-code', label: 'Claude Code CLI', kind: 'agentCli', available: true, detail: '2.0.14', needsSecret: false },
  { id: 'codex', label: 'Codex CLI', kind: 'agentCli', available: true, detail: '0.48.0', needsSecret: false },
  { id: 'system', label: '系統內建', kind: 'system', available: false, detail: '這個平台沒有可用的系統模型', needsSecret: false },
  { id: 'fixture', label: 'Fixture（測試用）', kind: 'fixture', available: true, detail: '固定輸出，不呼叫任何模型', needsSecret: false },
];

/* ── 假的 Tauri IPC ───────────────────────────────────────────── */

const RESPONSES = {
  resync: {
    state: 'recording',
    seq: 24,
    meetingTimeMs: 109_000,
    capturedAudioMs: 109_000,
    segments,
    speakers: SPEAKERS,
    notes: NOTES,
    snapshots: [
      { version: 1, throughEventSeq: 22, meetingTimeMs: 106_000, state: 'completed', prompt: '整理決議與待辦' },
    ],
    pauses: [],
  },
  // 歷史分頁截的是一場已經結束的會議。設成進行中的話，畫面上方會蓋一條
  // 「這場會議正在進行」的橫幅，那不是這張圖要講的事。
  active_meeting: null,
  list_meetings: OTHER_MEETINGS,
  open_meeting: DETAIL,
  snapshot_document: DOCUMENT,
  get_settings: SETTINGS,
  list_llm_backends: BACKENDS,
  search_meetings: [],
};

function ipcMock(responses) {
  const callbacks = new Map();
  let nextId = 1;
  window.__TAURI_INTERNALS__ = {
    transformCallback(cb) {
      const id = nextId++;
      callbacks.set(id, cb);
      return id;
    },
    unregisterCallback(id) {
      callbacks.delete(id);
    },
    convertFileSrc: (p) => p,
    async invoke(cmd, args) {
      if (cmd === 'plugin:event|listen') {
        window.__fireBatch = (batch) => {
          const cb = callbacks.get(args.handler);
          if (cb) cb({ event: args.event, id: args.handler, payload: batch });
        };
        return nextId++;
      }
      if (cmd === 'plugin:event|unlisten') return undefined;
      if (cmd in responses) return responses[cmd];
      return { accepted: true, seq: null, note: null };
    },
  };
}

/* ── 執行 ─────────────────────────────────────────────────────── */

const waitFor = async (fn, what, timeoutMs = 30_000) => {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await fn()) return;
    if (Date.now() > deadline) throw new Error(`等不到：${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
};

const vite = spawn('pnpm', ['exec', 'vite', '--port', String(PORT), '--strictPort'], {
  stdio: 'ignore',
});
process.on('exit', () => vite.kill());

try {
  await mkdir(OUT, { recursive: true });
  await waitFor(
    () => fetch(`http://localhost:${PORT}/`).then(() => true).catch(() => false),
    'vite 起來',
  );

  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: VIEWPORT, deviceScaleFactor: 2 });
  await page.addInitScript(
    ({ responses, src }) => {
      new Function('responses', `(${src})(responses)`)(responses);
    },
    { responses: RESPONSES, src: ipcMock.toString() },
  );
  await page.goto(`http://localhost:${PORT}/`);

  // 錄音：送一批事件進去，畫面就照正常路徑長出逐字稿
  await page.waitForSelector('.topbar');
  await waitFor(() => page.evaluate(() => typeof window.__fireBatch === 'function'), '訂閱建立');
  await page.evaluate((b) => window.__fireBatch(b), BATCH);
  await page.waitForSelector('text=' + LINES[0][1].slice(0, 12));
  await page.screenshot({ path: `${OUT}/live.png` });

  // 結束會議，再看歷史：這是使用者真正的順序
  await page.evaluate((b) => window.__fireBatch(b), STOPPED);

  // 歷史：清單加上重新打開的逐字稿
  await page.getByRole('tab', { name: '歷史' }).click();
  await page.waitForSelector('.mrow-main');
  await page.locator('.mrow-main').first().click();
  await page.waitForSelector('.hist-block');
  await page.screenshot({ path: `${OUT}/history.png` });

  // 成果文件：帶引用的那一版，這是這個產品跟一般摘要工具差最多的地方
  await page.getByRole('button', { name: '看摘要' }).click();
  await page.waitForSelector('.doc-tldr');
  await page.screenshot({ path: `${OUT}/document.png` });

  // 設定：後端偵測結果。等的是 select 本身，因為 option 在 Playwright 眼中是隱藏的
  await page.getByRole('tab', { name: '設定' }).click();
  await page.waitForSelector('.setting-grid select');
  await page.screenshot({ path: `${OUT}/settings.png` });

  await browser.close();
  console.log(`寫出 ${OUT}/live.png、history.png、document.png、settings.png`);
} finally {
  vite.kill();
}
