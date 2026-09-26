import { isIPv4, isIPv6 } from "node:net";

/**
 * The key a client address is blocked under: an IPv4 address as it is, an IPv6 address by its /64 prefix, since one
 * client usually holds a whole /64. IPv4-mapped IPv6 addresses count as IPv4. Undefined for anything else.
 */
export function blockKey(address: string): string | undefined {
  if (isIPv4(address)) return address;
  if (!isIPv6(address)) return undefined;
  const groups = expandIPv6(address.toLowerCase());
  if (groups.slice(0, 5).every((group) => group === 0) && groups[5] === 0xffff) {
    const [high = 0, low = 0] = groups.slice(6);
    return [high >> 8, high & 0xff, low >> 8, low & 0xff].join(".");
  }
  return `${groups
    .slice(0, 4)
    .map((group) => group.toString(16))
    .join(":")}::/64`;
}

function expandIPv6(address: string): number[] {
  let text = address;
  const dotted = /(\d+\.\d+\.\d+\.\d+)$/.exec(text)?.[1];
  if (dotted) {
    const [a = 0, b = 0, c = 0, d = 0] = dotted.split(".").map(Number);
    text = `${text.slice(0, -dotted.length)}${((a << 8) | b).toString(16)}:${((c << 8) | d).toString(16)}`;
  }
  const [head = "", tail] = text.split("::");
  const parse = (part: string) => (part ? part.split(":").map((group) => Number.parseInt(group, 16)) : []);
  const left = parse(head);
  const right = tail === undefined ? [] : parse(tail);
  return [...left, ...Array<number>(8 - left.length - right.length).fill(0), ...right];
}
