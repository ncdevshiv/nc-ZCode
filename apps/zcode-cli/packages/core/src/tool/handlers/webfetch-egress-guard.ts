import ipaddr from "ipaddr.js";
import { webFetchError } from "./webfetch-errors.js";

const IPV4_BENCHMARK_NETWORK = ipaddr.parseCIDR("198.18.0.0/15") as [ipaddr.IPv4, number];
const SPECIAL_USE_IPV6_NETWORKS: Array<[ipaddr.IPv6, number]> = [
  ipaddr.parseCIDR("64:ff9b:1::/48") as [ipaddr.IPv6, number],
  ipaddr.parseCIDR("100::/64") as [ipaddr.IPv6, number],
  ipaddr.parseCIDR("2001:2::/48") as [ipaddr.IPv6, number],
  ipaddr.parseCIDR("2001:10::/28") as [ipaddr.IPv6, number],
  ipaddr.parseCIDR("2001:20::/28") as [ipaddr.IPv6, number],
];
const DNS64_WELL_KNOWN_PREFIX = [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0];

export function assertWebFetchLiteralEgress(url: URL): void {
  const hostname = normalizeHostname(url.hostname);

  if (isLocalHostname(hostname)) {
    throw webFetchError("EgressBlocked", "WebFetch cannot access private or local hostnames", {
      hostname,
      url: url.toString(),
    });
  }

// DNS preflight cannot be completed within 1s under some networks, which will cause the public URL to fail before the actual fetch.
// Currently, only local/private network target blocking at the URL literal level is retained, and local DNS resolution for ordinary domain names is not performed.
  if (!isIpLiteral(hostname)) return;
  assertPublicIpAddress(hostname, { hostname, url });
}

function assertPublicIpAddress(address: string, context: { hostname: string; url: URL }): void {
  if (isPublicIpAddress(address)) return;

  throw webFetchError("EgressBlocked", "WebFetch cannot access private or local IP addresses", {
    address,
    hostname: context.hostname,
    url: context.url.toString(),
  });
}

function isPublicIpAddress(address: string): boolean {
  const parsed = parseIpAddress(address);
  if (!parsed) return false;

  const effectiveAddress = unwrapIpv6CarrierAddress(parsed) ?? parsed;
  if (isIpv4Address(effectiveAddress)) return isPublicIpv4(effectiveAddress);
  if (isIpv6Address(effectiveAddress)) return isPublicIpv6(effectiveAddress);
  return false;
}

function normalizeHostname(hostname: string): string {
  const trimmed = hostname.trim().toLowerCase();
  const withoutBrackets =
    trimmed.startsWith("[") && trimmed.endsWith("]") ? trimmed.slice(1, -1) : trimmed;
  return withoutBrackets.endsWith(".") ? withoutBrackets.slice(0, -1) : withoutBrackets;
}

export function isWebFetchIpLiteral(hostname: string): boolean {
  return isIpLiteral(normalizeHostname(hostname));
}

function isIpLiteral(hostname: string): boolean {
  return ipaddr.isValid(hostname);
}

function isLocalHostname(hostname: string): boolean {
  return hostname === "localhost" || hostname.endsWith(".localhost");
}

function parseIpAddress(address: string): ipaddr.IPv4 | ipaddr.IPv6 | undefined {
  try {
    return ipaddr.parse(normalizeHostname(address));
  } catch {
    return undefined;
  }
}

function unwrapIpv6CarrierAddress(address: ipaddr.IPv4 | ipaddr.IPv6): ipaddr.IPv4 | undefined {
  if (!isIpv6Address(address)) return undefined;
  if (address.isIPv4MappedAddress()) return address.toIPv4Address();

// The well-known prefix of NAT64/DNS64 will encode the IPv4 private network address into IPv6;
//You must first restore the lower 32-bit IPv4 and then apply the same public egress policy.
  const bytes = address.toByteArray();
  if (bytes.length !== 16) return undefined;
  if (!DNS64_WELL_KNOWN_PREFIX.every((byte, index) => bytes[index] === byte)) return undefined;
  return ipaddr.fromByteArray(bytes.slice(12)) as ipaddr.IPv4;
}

function isPublicIpv4(address: ipaddr.IPv4): boolean {
  if (address.match(IPV4_BENCHMARK_NETWORK)) return false;
  return address.range() === "unicast";
}

function isPublicIpv6(address: ipaddr.IPv6): boolean {
// ipaddr.js will classify some special-use IPv6 prefixes as unicast;
// The public egress boundary of WebFetch needs to explicitly exclude the address segments of these non-common public network targets.
  if (SPECIAL_USE_IPV6_NETWORKS.some((network) => address.match(network))) return false;
  return address.range() === "unicast";
}

function isIpv4Address(address: ipaddr.IPv4 | ipaddr.IPv6): address is ipaddr.IPv4 {
  return address.kind() === "ipv4";
}

function isIpv6Address(address: ipaddr.IPv4 | ipaddr.IPv6): address is ipaddr.IPv6 {
  return address.kind() === "ipv6";
}
