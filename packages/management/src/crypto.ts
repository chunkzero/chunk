import {
  createCipheriv,
  createDecipheriv,
  createHash,
  createHmac,
  hkdfSync,
  randomBytes,
  timingSafeEqual,
} from "node:crypto";

/** Encrypts secret values at rest. `context` binds a ciphertext to the record that holds it. */
export interface SecretCipher {
  seal(plaintext: Uint8Array, context: string): Promise<Uint8Array>;
  open(ciphertext: Uint8Array, context: string): Promise<Uint8Array>;
}

/** Purpose-separated keys derived from the operator's key. */
export interface Keys {
  cipher: SecretCipher;
  /** Keyed digest for comparing requests without storing their contents. */
  fingerprint(data: Uint8Array): Uint8Array;
  sign(data: string): string;
  verify(data: string, signature: string): boolean;
}

const format = 1;
const nonceBytes = 12;
const tagBytes = 16;

export function deriveKeys(master: Uint8Array): Keys {
  const derive = (purpose: string) => Buffer.from(hkdfSync("sha256", master, "", `chunk.management/${purpose}`, 32));
  const secretKey = derive("secrets");
  const fingerprintKey = derive("fingerprints");
  const signingKey = derive("signatures");
  const sign = (data: string) => createHmac("sha256", signingKey).update(data).digest("base64url");
  return {
    cipher: {
      async seal(plaintext, context) {
        const nonce = randomBytes(nonceBytes);
        const cipher = createCipheriv("aes-256-gcm", secretKey, nonce).setAAD(Buffer.from(context));
        const body = Buffer.concat([cipher.update(plaintext), cipher.final()]);
        return Buffer.concat([Buffer.of(format), nonce, body, cipher.getAuthTag()]);
      },
      async open(sealed, context) {
        if (sealed[0] !== format || sealed.length < 1 + nonceBytes + tagBytes) {
          throw new Error("unsupported ciphertext");
        }
        const nonce = sealed.subarray(1, 1 + nonceBytes);
        const decipher = createDecipheriv("aes-256-gcm", secretKey, nonce).setAAD(Buffer.from(context));
        decipher.setAuthTag(sealed.subarray(sealed.length - tagBytes));
        return Buffer.concat([
          decipher.update(sealed.subarray(1 + nonceBytes, sealed.length - tagBytes)),
          decipher.final(),
        ]);
      },
    },
    fingerprint: (data) => createHmac("sha256", fingerprintKey).update(data).digest(),
    sign,
    verify(data, signature) {
      const expected = Buffer.from(sign(data));
      const actual = Buffer.from(signature);
      return expected.length === actual.length && timingSafeEqual(expected, actual);
    },
  };
}

export function sha256(data: string | Uint8Array): Buffer {
  return createHash("sha256").update(data).digest();
}

export function randomToken(bytes = 32): string {
  return randomBytes(bytes).toString("base64url");
}

export function newId(prefix: string): string {
  return `${prefix}_${randomBytes(12).toString("hex")}`;
}
