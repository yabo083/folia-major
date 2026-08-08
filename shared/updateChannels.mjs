// shared/updateChannels.mjs
// M10 自动更新：发布通道语义（CI 侧与测试共享）。
// 镜像 folia-major/electron/updateChannels.cjs 的 realeco/limo/cielo/internal 通道模型，
// 但面向 Tauri 官方 updater：通道不再走 electron-updater 的 GitHub feed，
// 而是每个通道一个静态 manifest 端点（latest.json / beta.json / alpha.json），
// 由 CI 生成并注入端点 URL（构建期，见 src-tauri/src/updater.rs）。
//
// 通道语义（stable / beta / nightly 即 realeco / limo / cielo）：
//   - realeco（stable）→ latest.json，只允许稳定版；
//   - limo（beta）    → beta.json，允许稳定版 + beta 预发布（拒绝 alpha）；
//   - cielo（nightly）→ alpha.json，允许任何版本；
//   - internal        → 不发布、不更新。
// buildChannelManifest 是 CI 侧"通道门禁"：把版本推入错误通道会直接抛错（fail fast）。

const RELEASE_CHANNELS = {
  realeco: {
    id: 'realeco',
    label: 'Realeco',
    updaterChannel: 'latest',
    allowPrerelease: false,
    updateEnabled: true,
    rollingReleaseTag: null,
  },
  limo: {
    id: 'limo',
    label: 'Limo',
    updaterChannel: 'beta',
    allowPrerelease: true,
    updateEnabled: true,
    rollingReleaseTag: 'limo',
  },
  cielo: {
    id: 'cielo',
    label: 'Cielo',
    updaterChannel: 'alpha',
    allowPrerelease: true,
    updateEnabled: true,
    rollingReleaseTag: 'cielo',
  },
  internal: {
    id: 'internal',
    label: 'Internal',
    updaterChannel: null,
    allowPrerelease: false,
    updateEnabled: false,
    rollingReleaseTag: null,
  },
};

function normalizeReleaseChannel(value) {
  return typeof value === 'string' ? value.trim().toLowerCase() : '';
}

function resolveReleaseChannel(version, declaredChannel) {
  const declared = normalizeReleaseChannel(declaredChannel);
  if (RELEASE_CHANNELS[declared]) {
    return RELEASE_CHANNELS[declared];
  }

  const normalizedVersion = typeof version === 'string' ? version.toLowerCase() : '';
  if (/-alpha(?:[.\-]|$)/.test(normalizedVersion)) {
    return RELEASE_CHANNELS.cielo;
  }
  if (/-beta(?:[.\-]|$)/.test(normalizedVersion)) {
    return RELEASE_CHANNELS.limo;
  }
  return RELEASE_CHANNELS.realeco;
}

function getReleaseUrl(channel, version, releasesUrl) {
  const release = resolveReleaseChannel(version, channel);
  if (release.rollingReleaseTag) {
    return `${releasesUrl}/tag/${release.rollingReleaseTag}`;
  }

  const normalizedVersion = typeof version === 'string' ? version.trim().replace(/^v/i, '') : '';
  return normalizedVersion ? `${releasesUrl}/tag/v${normalizedVersion}` : releasesUrl;
}

// 通道 manifest 文件名：realeco→latest.json、limo→beta.json、cielo→alpha.json，internal→null。
function channelManifestName(channelId) {
  const channel = RELEASE_CHANNELS[normalizeReleaseChannel(channelId)];
  return channel && channel.updaterChannel ? `${channel.updaterChannel}.json` : null;
}

function isPrereleaseVersion(version) {
  return /-\d*[0-9A-Za-z-][0-9A-Za-z-]*(?:\.[0-9A-Za-z-]+)*$/.test(
    typeof version === 'string' ? version.trim() : '',
  );
}

function isAlphaPrerelease(version) {
  return /-alpha(?:[.\-]|$)/i.test(typeof version === 'string' ? version.trim() : '');
}

// 通道版本门禁（与 src-tauri/src/updater.rs 的 channel_allows_version 保持一致）：
// realeco 拒预发布；limo 拒 alpha 预发布；cielo 全放行。
function isChannelAllowedVersion(channelId, version) {
  const channel = RELEASE_CHANNELS[normalizeReleaseChannel(channelId)];
  if (!channel || !channel.updateEnabled) {
    return false;
  }
  if (!isPrereleaseVersion(version)) {
    return true;
  }
  if (channel.id === 'cielo') {
    return true;
  }
  if (channel.id === 'limo') {
    return !isAlphaPrerelease(version);
  }
  return false;
}

const SEMVER_PATTERN = /^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

// 生成 Tauri v2 updater 端点 manifest（静态格式）：
// { version, notes, pub_date, platforms: { "windows-x86_64": { signature, url } } }。
// 通道门禁：版本不属于该通道时抛错（防止 alpha 被推进 latest.json 等跨通道事故）。
function buildChannelManifest({ channel, version, signature, url, notes = null, pubDate = null }) {
  const releaseChannel = RELEASE_CHANNELS[normalizeReleaseChannel(channel)];
  if (!releaseChannel || !releaseChannel.updateEnabled) {
    throw new Error(`Cannot publish to update-disabled channel: ${String(channel)}`);
  }
  const normalizedVersion = typeof version === 'string' ? version.trim().replace(/^v/i, '') : '';
  if (!SEMVER_PATTERN.test(normalizedVersion)) {
    throw new Error(`Invalid version for update manifest: ${String(version)}`);
  }
  if (typeof signature !== 'string' || !signature.trim()) {
    throw new Error('Update manifest requires a non-empty signature.');
  }
  const normalizedUrl = typeof url === 'string' ? url.trim() : '';
  if (!/^https:\/\//i.test(normalizedUrl)) {
    throw new Error(`Update manifest url must be https: ${String(url)}`);
  }
  if (!isChannelAllowedVersion(releaseChannel.id, normalizedVersion)) {
    throw new Error(
      `Version ${normalizedVersion} is not allowed on channel ${releaseChannel.id} ` +
        `(stable rejects prereleases; beta rejects alpha prereleases).`,
    );
  }

  const manifest = {
    version: normalizedVersion,
    pub_date: pubDate || new Date().toISOString(),
    platforms: {
      'windows-x86_64': {
        signature: signature.trim(),
        url: normalizedUrl,
      },
    },
  };
  if (typeof notes === 'string' && notes.trim()) {
    manifest.notes = notes;
  }
  return manifest;
}

export {
  RELEASE_CHANNELS,
  buildChannelManifest,
  channelManifestName,
  getReleaseUrl,
  isAlphaPrerelease,
  isChannelAllowedVersion,
  isPrereleaseVersion,
  resolveReleaseChannel,
};
