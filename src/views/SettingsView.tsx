/**
 * Provider 設定（§5.6）。
 *
 * 三件事必須在畫面上成立，不只在後端成立：
 *
 * 1. 被環境變數決定的欄位標成唯讀。讓使用者對著一個改了也沒用的輸入框
 *    調整，比不給他改更糟。
 * 2. 密鑰只顯示狀態，永遠不回填。後端根本不回傳值，這裡也沒有地方接。
 * 3. 憑證庫不可用要與「還沒設密鑰」分開講，兩者要做的事不一樣。
 */
import { useCallback, useEffect, useState } from 'react';
import {
  settings,
  type BackendOption,
  type FieldSource,
  type ProviderKind,
  type ResolvedProvider,
  type SecretPresence,
} from '../session';

const KIND_LABEL: Record<ProviderKind, string> = {
  stt: '逐字稿 Provider',
  llm: '摘要與 Agent Provider',
};

const KIND_HINT: Record<ProviderKind, string> = {
  stt: '負責即時轉錄。切換 Provider 不影響已錄下的音訊與既有逐字稿。',
  llm: '負責摘要快照與 Agent Loop。生成失敗時錄音與逐字稿不受影響。',
};

const SECRET_LABEL: Record<SecretPresence, string> = {
  environment: '由環境變數提供',
  keychain: '已存入系統憑證庫',
  missing: '尚未設定',
  unavailable: '憑證庫無法存取',
};

const SOURCE_LABEL: Record<FieldSource, string> = {
  environment: '環境變數',
  settings: '本機設定',
  default: '預設值',
};

/** 環境變數決定的欄位改了也不會生效，因此鎖住輸入框並說明原因。 */
function Field({
  label,
  value,
  source,
  placeholder,
  onChange,
}: {
  label: string;
  value: string;
  source: FieldSource;
  placeholder?: string;
  onChange: (v: string) => void;
}) {
  const locked = source === 'environment';
  return (
    <label className="setting-field">
      <span className="setting-label">
        {label}
        <em data-src={source}>{SOURCE_LABEL[source]}</em>
      </span>
      <input
        value={value}
        placeholder={placeholder}
        readOnly={locked}
        aria-readonly={locked}
        title={locked ? '這個欄位由環境變數決定，在這裡修改不會生效' : undefined}
        onChange={(e) => onChange(e.target.value)}
      />
    </label>
  );
}

/** 摘要與 Agent Provider 的後端選單。不可用的項目仍然列出但選不動。 */
function BackendPicker({
  value,
  source,
  options,
  onChange,
}: {
  value: string;
  source: FieldSource;
  options: BackendOption[];
  onChange: (v: string) => void;
}) {
  const locked = source === 'environment';
  // 環境變數可以指定一個不在偵測清單裡的值，那也要顯示出來，
  // 否則畫面上的選項與實際生效的設定不一致
  const known = options.some((o) => o.id === value);
  const chosen = options.find((o) => o.id === value);
  return (
    <label className="setting-field setting-wide">
      <span className="setting-label">
        後端
        <em data-src={source}>{SOURCE_LABEL[source]}</em>
      </span>
      <select
        value={value}
        disabled={locked}
        aria-disabled={locked}
        title={locked ? '這個欄位由環境變數決定，在這裡修改不會生效' : undefined}
        onChange={(e) => onChange(e.target.value)}
      >
        {!known && <option value={value}>{value}（環境變數指定）</option>}
        {options.map((o) => (
          <option key={o.id} value={o.id} disabled={!o.available}>
            {o.label}
            {o.available ? '' : '（無法使用）'}
          </option>
        ))}
      </select>
      <span className="hint" data-ok={chosen ? chosen.available : undefined}>
        {chosen?.detail ?? '偵測中。'}
      </span>
    </label>
  );
}

interface Draft {
  provider: string;
  model: string;
  baseUrl: string;
  secret: string;
}

export function SettingsView() {
  const [rows, setRows] = useState<ResolvedProvider[] | null>(null);
  const [backends, setBackends] = useState<BackendOption[]>([]);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [status, setStatus] = useState<{ tone: 'ok' | 'bad'; text: string } | null>(null);
  // null 代表還沒讀到。用它區分「載入中」與「已知關閉」，
  // 否則畫面會先閃一下錯的狀態，讓人以為設定被改掉了。
  const [keepAudio, setKeepAudio] = useState<boolean | null>(null);

  const reload = useCallback(async () => {
    try {
      // 偵測要啟動子行程，比讀設定慢，但兩者一起等：先畫出一個
      // 還不知道能不能用的選單，只會讓使用者選到一半又被換掉
      const [r, b, keep] = await Promise.all([
        settings.get(),
        settings.backends(),
        settings.keepAudio(),
      ]);
      setBackends(b);
      setRows(r);
      setKeepAudio(keep);
      // 草稿從已解析的值起始，包含環境變數提供的值：
      // 使用者看到的就是實際生效的內容
      setDrafts(
        Object.fromEntries(
          r.map((p) => [
            p.kind,
            { provider: p.provider.value, model: p.model.value, baseUrl: p.baseUrl.value, secret: '' },
          ]),
        ),
      );
    } catch (e) {
      setStatus({ tone: 'bad', text: String(e) });
      setRows([]);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const patch = (kind: ProviderKind, part: Partial<Draft>) =>
    setDrafts((d) => ({ ...d, [kind]: { ...d[kind], ...part } }));

  const save = async (kind: ProviderKind) => {
    const d = drafts[kind];
    if (!d) return;
    try {
      await settings.saveProvider(kind, d.provider, d.model, d.baseUrl);
      if (d.secret.trim()) {
        await settings.saveSecret(kind, d.secret);
        // 草稿裡的密鑰用完就清掉，不讓它留在記憶體裡等著被截圖
        patch(kind, { secret: '' });
      }
      await reload();
      setStatus({ tone: 'ok', text: `${KIND_LABEL[kind]}已儲存。` });
    } catch (e) {
      setStatus({ tone: 'bad', text: String(e) });
    }
  };

  const clearSecret = async (kind: ProviderKind) => {
    try {
      await settings.clearSecret(kind);
      await reload();
      setStatus({ tone: 'ok', text: '密鑰已從系統憑證庫移除。' });
    } catch (e) {
      setStatus({ tone: 'bad', text: String(e) });
    }
  };

  return (
    <div className="page page-single">
      {status && (
        <div className="banner" data-tone={status.tone === 'ok' ? 'warn' : 'bad'} role="status">
          <span>
            <b>{status.tone === 'ok' ? '已更新' : '沒有存成功'}</b>
            <p>{status.text}</p>
          </span>
          <button className="btn-ghost close" onClick={() => setStatus(null)} aria-label="關閉">
            ×
          </button>
        </div>
      )}

      {rows === null && <p className="hint">讀取設定中。</p>}

      {rows?.map((p) => {
        const d = drafts[p.kind];
        if (!d) return null;
        // llm 走偵測出來的後端選單；stt 目前只有 fixture 與未來的 Adapter，
        // 還沒有可偵測的對象，維持自由輸入
        const picker = p.kind === 'llm';
        const chosen = picker ? backends.find((o) => o.id === d.provider) : undefined;
        // 只有 API 後端要金鑰與端點。Agent CLI 用的是使用者在該 CLI 上
        // 已經完成的登入，這裡再要一次金鑰是多問的
        const needsKey = picker ? (chosen?.needsSecret ?? false) : true;
        return (
          <section className="panel" key={p.kind}>
            <div className="panel-head">
              <span className="card-title">{KIND_LABEL[p.kind]}</span>
              <span className="spacer" />
              <span className="secret-pill" data-s={p.secret}>
                {SECRET_LABEL[p.secret]}
              </span>
            </div>
            <p className="hint">{KIND_HINT[p.kind]}</p>

            <div className="setting-grid">
              {picker ? (
                <BackendPicker
                  value={d.provider}
                  source={p.provider.source}
                  options={backends}
                  onChange={(v) => patch(p.kind, { provider: v })}
                />
              ) : (
                <Field
                  label="Provider"
                  value={d.provider}
                  source={p.provider.source}
                  placeholder="fixture"
                  onChange={(v) => patch(p.kind, { provider: v })}
                />
              )}

              {needsKey && (
                <>
                  <Field
                    label="模型"
                    value={d.model}
                    source={p.model.source}
                    placeholder="留空使用 Provider 預設"
                    onChange={(v) => patch(p.kind, { model: v })}
                  />
                  <Field
                    label="Base URL"
                    value={d.baseUrl}
                    source={p.baseUrl.source}
                    placeholder="OpenAI-compatible 端點，留空使用官方"
                    onChange={(v) => patch(p.kind, { baseUrl: v })}
                  />

                  <label className="setting-field">
                    <span className="setting-label">
                      API Key
                      <em data-src={p.secret === 'environment' ? 'environment' : 'settings'}>
                        {SECRET_LABEL[p.secret]}
                      </em>
                    </span>
                    <input
                      type="password"
                      value={d.secret}
                      autoComplete="off"
                      placeholder={
                        p.secret === 'environment'
                          ? '環境變數已提供，這裡填的值不會被使用'
                          : p.secret === 'keychain'
                            ? '已設定。要更換請輸入新的值'
                            : '輸入後存入系統憑證庫'
                      }
                      readOnly={p.secret === 'environment'}
                      onChange={(e) => patch(p.kind, { secret: e.target.value })}
                    />
                  </label>
                </>
              )}
            </div>

            {picker && !needsKey && (
              <p className="hint">
                這個後端用你在該 CLI 上已完成的登入，不需要在這裡填金鑰。生成失敗時
                （未登入、額度用盡）會回報為 Provider 錯誤，錄音與逐字稿不受影響。
              </p>
            )}

            {needsKey && p.secret === 'unavailable' && (
              <div className="banner" data-tone="warn" role="status">
                <span>
                  <b>這台機器沒有可用的系統憑證庫</b>
                  <p>
                    密鑰不會被寫進設定檔或資料庫，因此這裡無法儲存。請改用環境變數
                    <code>OPENMEETNOTE_{p.kind.toUpperCase()}_API_KEY</code> 提供。
                  </p>
                </span>
              </div>
            )}

            <div className="setting-actions">
              <button className="btn btn-primary" onClick={() => void save(p.kind)}>
                儲存
              </button>
              {(needsKey || p.secret === 'keychain') && (
                <button
                  className="btn"
                  disabled={p.secret !== 'keychain'}
                  onClick={() => void clearSecret(p.kind)}
                >
                  移除密鑰
                </button>
              )}
            </div>
          </section>
        );
      })}

      <section className="panel">
        <div className="panel-head">
          <span className="card-title">錄音保存</span>
        </div>
        <label className="keep-audio">
          <input
            type="checkbox"
            checked={keepAudio ?? true}
            disabled={keepAudio === null}
            onChange={async (e) => {
              const next = e.target.checked;
              // 先寫後端再更新畫面：反過來的話寫入失敗時畫面會顯示
              // 一個沒有生效的狀態，而使用者以為已經關掉了
              try {
                await settings.setKeepAudio(next);
                setKeepAudio(next);
                setStatus({ tone: 'ok', text: next ? '之後的會議會保留原音' : '之後的會議不保留原音' });
              } catch (err) {
                setStatus({ tone: 'bad', text: String(err) });
              }
            }}
          />
          <span>保留原音（下一場會議開始時生效）</span>
        </label>
        <p className="hint">
          原音是驗證逐字稿的唯一依據：轉錯字或漏掉發言時，沒有它就只能憑印象判斷，
          也無法換模型重跑同一段話比較。代價是磁碟，兩軌約 230 MB/小時。
          單場的音檔可以到歷史頁個別刪除。
        </p>
      </section>

      <section className="panel">
        <div className="panel-head">
          <span className="card-title">密鑰存放位置</span>
        </div>
        <p className="hint">
          API Key 只存在 Windows Credential Manager 或 macOS Keychain，不會進入 SQLite、設定檔、
          逐字稿或日誌。環境變數的優先權高於這裡的設定，企業環境可以用它統一派送。
        </p>
      </section>
    </div>
  );
}
