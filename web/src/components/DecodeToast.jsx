import React, { useEffect } from 'react';
import { useStore } from '../store.js';

const AUTO_DISMISS_MS = 5000;

export default function DecodeToast() {
  const toast         = useStore(s => s.decodeToast);
  const clearToast    = useStore(s => s.clearDecodeToast);

  useEffect(() => {
    if (!toast) return;
    const id = setTimeout(clearToast, AUTO_DISMISS_MS);
    return () => clearTimeout(id);
  }, [toast, clearToast]);

  if (!toast) return null;

  return (
    <div className="decode-toast" role="alert" onClick={clearToast}>
      <span className="decode-toast-icon">✕</span>
      <span className="decode-toast-msg">Decode failed: {toast.message}</span>
    </div>
  );
}
