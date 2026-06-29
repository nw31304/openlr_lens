import React, { useRef, useEffect, useState } from 'react';
import { useStore } from '../store.js';
import { useDraggable } from '../hooks.js';
import { renderLlmText } from '../renderLlmText.jsx';

// Format hint appended to structured prompts sent to the API.
// Shown only to the model — the user sees their original words.
const STRUCTURED_FORMAT = `

Respond with:
- "What happened:" (one sentence)
- "Why:" (bullet list using - prefix, citing specific numbers from the data)
- "Suggestions:" (specific parameter changes using labels from "Active parameters"; omit entirely if the decode succeeded cleanly)`;

const SUGGESTED_FAIL = [
  'Why did this fail?',
  'What is the minimum parameter change to fix this?',
  'Is this an encoder or decoder problem?',
];

const SUGGESTED_OK = [
  'Summarise this decode',
  'Are there any suspicious aspects?',
  'Why was this route chosen over alternatives?',
];

// Prompts that get the structured format hint appended before sending to the API
const STRUCTURED_PROMPTS = new Set([
  ...SUGGESTED_FAIL,
  ...SUGGESTED_OK,
]);

function fmtBytes(n) {
  if (n < 1000) return `${n} B`;
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)} kB`;
  return `${(n / 1_000_000).toFixed(1)} MB`;
}

export default function LlmChatPanel() {
  const { llmChatOpen, toggleLlmChat, llmMessages, llmLoading,
          sendLlmMessage, clearLlmChat, llmConfig, decodeResult,
          llmLastToolActivity } = useStore();
  const [draft, setDraft] = useState('');
  const bottomRef = useRef(null);
  const panelRef  = useRef(null);
  const inputRef  = useRef(null);
  const { pos, onMouseDown } = useDraggable(panelRef);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [llmMessages, llmLoading]);

  if (!llmChatOpen) return null;

  const suggestions = decodeResult?.ok ? SUGGESTED_OK : SUGGESTED_FAIL;
  const isEmpty = llmMessages.length === 0 && !llmLoading;
  const modelLabel = llmConfig?.model ?? '';

  async function send(text) {
    const t = text.trim();
    if (!t || llmLoading) return;
    setDraft('');
    const content = STRUCTURED_PROMPTS.has(t) ? t + STRUCTURED_FORMAT : t;
    await sendLlmMessage(content, t);
    inputRef.current?.focus();
  }

  const panelStyle = pos
    ? { left: pos.left, top: pos.top, right: 'auto' }
    : undefined;

  return (
    <div ref={panelRef} className="llm-chat-panel" style={panelStyle}>
      <div className="llm-chat-header draggable-header" onMouseDown={onMouseDown}>
        <span className="llm-chat-title">✦ AI Chat{modelLabel ? ` · ${modelLabel}` : ''}</span>
        <div className="llm-chat-header-btns">
          {llmMessages.length > 0 && (
            <button className="llm-chat-clear" onClick={clearLlmChat} title="Clear conversation">↺</button>
          )}
          <button className="seg-info-close" onClick={toggleLlmChat} title="Close">✕</button>
        </div>
      </div>

      {llmLastToolActivity && (
        <div
          className="llm-tool-strip"
          title={llmLastToolActivity.calls.map(c =>
            `${c.label}  ↑${fmtBytes(c.args_bytes)} ↓${fmtBytes(c.result_bytes)}`
          ).join('\n')}
        >
          <span className="llm-tool-strip-icon">⚙</span>
          <span className="llm-tool-strip-calls">
            {llmLastToolActivity.calls.map(c => c.label).join(' · ')}
          </span>
          <span className="llm-tool-strip-bytes">
            ↓{fmtBytes(llmLastToolActivity.total_result_bytes)}
          </span>
        </div>
      )}

      <div className="llm-chat-body">
        {isEmpty && (
          <div className="llm-chat-empty">
            <div className="llm-chat-empty-label">Suggested</div>
            <div className="llm-chat-chips">
              {suggestions.map(s => (
                <button key={s} className="llm-chip" onClick={() => send(s)}>{s}</button>
              ))}
            </div>
          </div>
        )}

        {llmMessages.map((msg, i) => (
          <div key={i} className={`llm-msg llm-msg-${msg.role}`}>
            {msg.role === 'assistant' ? (
              <div className={`llm-msg-bubble ${msg.error ? 'llm-msg-error' : ''}`}>
                {renderLlmText(msg.content)}
              </div>
            ) : (
              <div className="llm-msg-bubble llm-msg-user-bubble">{msg.display ?? msg.content}</div>
            )}
          </div>
        ))}

        {llmLoading && (
          <div className="llm-msg llm-msg-assistant">
            <div className="llm-msg-bubble llm-typing">
              <span /><span /><span />
            </div>
          </div>
        )}
        <div ref={bottomRef} />
      </div>

      {!isEmpty && (
        <div className="llm-chat-chips llm-chat-chips-inline">
          {suggestions.map(s => (
            <button key={s} className="llm-chip llm-chip-sm" onClick={() => send(s)} disabled={llmLoading}>{s}</button>
          ))}
        </div>
      )}

      <div className="llm-chat-input-row">
        <input
          ref={inputRef}
          className="llm-chat-input"
          type="text"
          placeholder="Ask a question…"
          value={draft}
          onChange={e => setDraft(e.target.value)}
          onKeyDown={e => e.key === 'Enter' && send(draft)}
          disabled={llmLoading}
        />
        <button
          className="llm-chat-send"
          onClick={() => send(draft)}
          disabled={llmLoading || !draft.trim()}
        >→</button>
      </div>
    </div>
  );
}
