// WebCrypto's `BufferSource` parameter type is `ArrayBufferView<ArrayBuffer> |
// ArrayBuffer`. Since TypeScript 5.7 a bare `Uint8Array` is
// `Uint8Array<ArrayBufferLike>` (the buffer might be a `SharedArrayBuffer`), so
// passing one straight into `crypto.subtle.*` no longer type-checks. `bs()`
// returns a copy guaranteed to sit on a plain `ArrayBuffer`.

export function bs(u: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(new ArrayBuffer(u.byteLength));
  out.set(u);
  return out;
}
