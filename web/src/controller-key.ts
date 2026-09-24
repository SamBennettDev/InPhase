// Per-browser-profile controller identity.
//
// One browser profile == one paired device == a non-extractable Ed25519 signing
// key in IndexedDB. Its public half is the controller id the host pins in its
// ACL and approves once, locally. The browser signs a fresh challenge with it on
// every signaling connect.
//
// Clearing site data deletes the key: an explicit loss-of-controller event —
// the device must be paired again.
//
// Needs WebCrypto Ed25519 (current Chrome/Edge, Firefox, Safari). Available only
// in a secure context, which is why the host serves HTTPS.

const DB_NAME = "inphase";
const STORE = "identity";
const KEY = "controller";

interface StoredPair {
  signPublic: CryptoKey;
  signPrivate: CryptoKey;
}

export interface ControllerIdentity {
  /** lowercase hex of the 32-byte Ed25519 public key — the controller id. */
  publicKeyHex: string;
  /** Ed25519 signature over arbitrary data (the connect challenge). */
  sign(data: BufferSource): Promise<Uint8Array>;
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains(STORE)) req.result.createObjectStore(STORE);
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function idb<T>(mode: IDBTransactionMode, fn: (s: IDBObjectStore) => IDBRequest): Promise<T> {
  const db = await openDb();
  try {
    return await new Promise<T>((resolve, reject) => {
      const r = fn(db.transaction(STORE, mode).objectStore(STORE));
      r.onsuccess = () => resolve(r.result as T);
      r.onerror = () => reject(r.error);
    });
  } finally {
    db.close();
  }
}

async function generatePair(): Promise<StoredPair | null> {
  try {
    const sign = (await crypto.subtle.generateKey({ name: "Ed25519" }, false, [
      "sign",
      "verify",
    ])) as CryptoKeyPair;
    return { signPublic: sign.publicKey, signPrivate: sign.privateKey };
  } catch {
    return null; // browser lacks WebCrypto Ed25519
  }
}

function hex(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += x.toString(16).padStart(2, "0");
  return s;
}

function isPair(v: unknown): v is StoredPair {
  const p = v as Partial<StoredPair> | undefined;
  return !!p?.signPrivate && !!p.signPublic;
}

let cached: Promise<ControllerIdentity | null> | null = null;

/** Load the existing key, or create + persist it. Returns null if the browser
 *  has no usable WebCrypto Ed25519 (or no secure context). */
export function getControllerIdentity(): Promise<ControllerIdentity | null> {
  if (cached) return cached;
  cached = (async () => {
    if (!globalThis.crypto?.subtle || !globalThis.indexedDB) return null;
    let pair: StoredPair | null = null;
    try {
      const got = await idb<unknown>("readonly", (s) => s.get(KEY));
      if (isPair(got)) pair = got;
    } catch {
      /* private mode / blocked */
    }
    if (!pair) {
      pair = await generatePair();
      if (!pair) return null;
      try {
        await idb("readwrite", (s) => s.put(pair, KEY));
      } catch {
        /* can't persist — key is session-only, still usable */
      }
    }

    const signRaw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.signPublic));
    const signPrivate = pair.signPrivate;
    return {
      publicKeyHex: hex(signRaw),
      sign: async (data: BufferSource) =>
        new Uint8Array(await crypto.subtle.sign({ name: "Ed25519" }, signPrivate, data)),
    };
  })();
  return cached;
}

/** Forget this device — delete the key so the browser must pair again. */
export async function forgetControllerIdentity(): Promise<void> {
  cached = null;
  try {
    await idb("readwrite", (s) => s.delete(KEY));
  } catch {
    /* nothing to delete */
  }
}

/** A friendly label the Host stores with the controller ("Chrome on Windows"). */
export function deviceLabel(): string {
  const ua = navigator.userAgent;
  const brand =
    (navigator as Navigator & { userAgentData?: { brands?: { brand: string }[] } }).userAgentData
      ?.brands?.find((b) => !/Not.?A.?Brand/i.test(b.brand))?.brand ??
    (/\bEdg\//.test(ua)
      ? "Edge"
      : /\bFirefox\//.test(ua)
        ? "Firefox"
        : /\bChrome\//.test(ua)
          ? "Chrome"
          : /\bSafari\//.test(ua)
            ? "Safari"
            : "Browser");
  const os = /Windows/.test(ua)
    ? "Windows"
    : /Mac OS X|Macintosh/.test(ua)
      ? "macOS"
      : /Android/.test(ua)
        ? "Android"
        : /iPhone|iPad/.test(ua)
          ? "iOS"
          : /Linux/.test(ua)
            ? "Linux"
            : "";
  return os ? `${brand} on ${os}` : brand;
}
