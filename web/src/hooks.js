import { useState, useRef, useEffect } from 'react';

/**
 * Makes a panel draggable by its header element.
 *
 * Returns { pos, onMouseDown } where:
 *   pos = null (use CSS defaults) | { left, top } (panel has been dragged)
 *   onMouseDown = attach to the drag handle element
 */
export function useDraggable(panelRef) {
  const [pos, setPos] = useState(null);
  const dragState = useRef(null);

  useEffect(() => {
    const onMove = (e) => {
      if (!dragState.current) return;
      const { startX, startY, initLeft, initTop } = dragState.current;
      setPos({ left: initLeft + e.clientX - startX, top: initTop + e.clientY - startY });
    };
    const onUp = () => {
      if (!dragState.current) return;
      dragState.current = null;
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
    return () => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
    };
  }, []);

  const onMouseDown = (e) => {
    if (e.button !== 0) return;
    const rect = panelRef.current?.getBoundingClientRect();
    if (!rect) return;
    dragState.current = { startX: e.clientX, startY: e.clientY, initLeft: rect.left, initTop: rect.top };
    document.body.style.cursor = 'grabbing';
    document.body.style.userSelect = 'none';
    e.preventDefault();
  };

  return { pos, onMouseDown, resetPos: () => setPos(null) };
}
