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

interface Draft {
  provider: string;
  model: string;
  baseUrl: string;
  secret: string;
}

export function SettingsView() {
  const [rows, setRows] = useState<ResolvedProvider[] | null>(null);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [status, setStatus] = useState<{ tone: 'ok' | 'bad'; text: string } | null>(null);

  const reload = useCallback(async () => {
    try {
      const r = await settings.get();
      setRows(r);
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
              <Field
                label="Provider"
                value={d.provider}
                source={p.provider.source}
                placeholder="fixture"
                onChange={(v) => patch(p.kind, { provider: v })}
              />
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
            </div>

            {p.secret === 'unavailable' && (
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
              <button
                className="btn"
                disabled={p.secret !== 'keychain'}
                onClick={() => void clearSecret(p.kind)}
              >
                移除密鑰
              </button>
            </div>
          </section>
        );
      })}

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
