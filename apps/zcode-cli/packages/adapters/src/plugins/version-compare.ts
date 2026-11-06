import semver from "semver";

export type PluginUpdateStatus = "none" | "update-available" | "version-changed";

/**
 * Compare an installed plugin version against the latest manifest version.
 * - semver.gt(latest, installed) -> "update-available"
 * - parseable + not greater (equal or older) -> "none"
 * - unparseable but differing -> "version-changed" (we can't prove it's newer)
 * - unparseable + equal, or either missing -> "none"
 */
export function comparePluginVersions(input: {
  installed: string | undefined;
  latest: string | undefined;
}): PluginUpdateStatus {
  const { installed, latest } = input;
  if (!installed || !latest) return "none";
  const installedValid = semver.valid(semver.coerce(installed) ?? installed);
  const latestValid = semver.valid(semver.coerce(latest) ?? latest);
  if (installedValid && latestValid) {
    return semver.gt(latestValid, installedValid) ? "update-available" : "none";
  }
  return installed === latest ? "none" : "version-changed";
}

/**
* Determine the update status according to the comparison axis actually provided by the latest directory entry: compare the parsable version first, and compare when missing
* source identity pin; keep none when both are missing to avoid falsely reporting unproven old and new relationships as updates.
 */
export function comparePluginUpdate(input: {
  installedVersion: string | undefined;
  installedSha: string | undefined;
  latestVersion: string | undefined;
  latestSha: string | undefined;
}): PluginUpdateStatus {
  const { installedVersion, installedSha, latestVersion, latestSha } = input;
  if (latestVersion) {
    return comparePluginVersions({ installed: installedVersion, latest: latestVersion });
  }
  if (latestSha) {
    if (!installedSha) return "version-changed";
    return installedSha === latestSha ? "none" : "update-available";
  }
  return "none";
}
