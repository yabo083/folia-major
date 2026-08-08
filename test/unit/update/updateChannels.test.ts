import { describe, expect, it } from 'vitest';

// test/unit/update/updateChannels.test.ts
// M10 自动更新：发布通道语义（规格来源 folia-major/test/unit/electron/updateChannels.test.ts）。

const {
    RELEASE_CHANNELS,
    buildChannelManifest,
    channelManifestName,
    getReleaseUrl,
    isChannelAllowedVersion,
    resolveReleaseChannel,
} = require('../../../shared/updateChannels.mjs') as {
    RELEASE_CHANNELS: Record<string, {
        id: string;
        label: string;
        updaterChannel: string | null;
        allowPrerelease: boolean;
        updateEnabled: boolean;
        rollingReleaseTag: string | null;
    }>;
    buildChannelManifest: (input: {
        channel: string;
        version: string;
        signature: string;
        url: string;
        notes?: string | null;
        pubDate?: string | null;
    }) => Record<string, unknown>;
    channelManifestName: (channel: string) => string | null;
    getReleaseUrl: (channel: string, version: string, releasesUrl: string) => string;
    isChannelAllowedVersion: (channel: string, version: string) => boolean;
    resolveReleaseChannel: (version: string, declaredChannel?: string | null) => {
        id: string;
        updaterChannel: string | null;
        allowPrerelease: boolean;
        updateEnabled: boolean;
        rollingReleaseTag: string | null;
    };
};

describe('release update channels', () => {
    it('uses packaged metadata before inferring a legacy version suffix', () => {
        expect(resolveReleaseChannel('0.7.0-beta.1', 'internal')).toMatchObject({
            id: 'internal',
            updaterChannel: null,
            updateEnabled: false,
        });
    });

    it.each([
        ['0.7.0', 'realeco', 'latest', false],
        ['0.7.0-beta.123', 'limo', 'beta', true],
        ['0.7.0-alpha.123', 'cielo', 'alpha', true],
    ])('maps %s to the %s lane', (version, id, updaterChannel, allowPrerelease) => {
        expect(resolveReleaseChannel(version)).toMatchObject({ id, updaterChannel, allowPrerelease });
    });

    it('opens rolling prereleases instead of manufacturing a semver tag', () => {
        const releasesUrl = 'https://github.com/chthollyphile/folia-major/releases';

        expect(getReleaseUrl('limo', '0.7.0-beta.123', releasesUrl)).toBe(`${releasesUrl}/tag/limo`);
        expect(getReleaseUrl('cielo', '0.7.0-alpha.123', releasesUrl)).toBe(`${releasesUrl}/tag/cielo`);
        expect(getReleaseUrl('realeco', '0.7.0', releasesUrl)).toBe(`${releasesUrl}/tag/v0.7.0`);
    });

    it('exposes one endpoint manifest per updater channel', () => {
        expect(channelManifestName('realeco')).toBe('latest.json');
        expect(channelManifestName('limo')).toBe('beta.json');
        expect(channelManifestName('cielo')).toBe('alpha.json');
        expect(channelManifestName('internal')).toBeNull();
        expect(channelManifestName('unknown')).toBeNull();
    });

    it.each([
        ['realeco', '0.7.0', true],
        ['realeco', '0.7.0-beta.1', false],
        ['realeco', '0.7.0-alpha.1', false],
        ['limo', '0.7.0', true],
        ['limo', '0.7.0-beta.1', true],
        ['limo', '0.7.0-alpha.1', false],
        ['cielo', '0.7.0', true],
        ['cielo', '0.7.0-beta.1', true],
        ['cielo', '0.7.0-alpha.1', true],
        ['internal', '0.7.0', false],
    ])('channel %s allows %s: %s', (channel, version, allowed) => {
        expect(isChannelAllowedVersion(channel, version)).toBe(allowed);
    });
});

describe('update endpoint manifest generation', () => {
    const signature = 'dW50cnVzdGVkLXNpZ25hdHVyZQ==';

    it('builds the static Tauri v2 endpoint manifest shape', () => {
        const manifest = buildChannelManifest({
            channel: 'realeco',
            version: '0.7.0',
            signature,
            url: 'https://releases.example.com/Folia_0.7.0_x64-setup.exe',
            notes: 'release notes',
            pubDate: '2026-08-07T00:00:00Z',
        });
        expect(manifest).toMatchObject({
            version: '0.7.0',
            notes: 'release notes',
            pub_date: '2026-08-07T00:00:00Z',
            platforms: {
                'windows-x86_64': {
                    signature,
                    url: 'https://releases.example.com/Folia_0.7.0_x64-setup.exe',
                },
            },
        });
    });

    it('strips a leading v from the version', () => {
        const manifest = buildChannelManifest({
            channel: 'limo',
            version: 'v0.7.0-beta.1',
            signature,
            url: 'https://releases.example.com/Folia_0.7.0_x64-setup.exe',
        });
        expect(manifest.version).toBe('0.7.0-beta.1');
    });

    it('refuses to push a prerelease into the stable manifest', () => {
        expect(() => buildChannelManifest({
            channel: 'realeco',
            version: '0.7.0-beta.1',
            signature,
            url: 'https://releases.example.com/Folia_0.7.0_x64-setup.exe',
        })).toThrow(/not allowed on channel realeco/);
    });

    it('refuses to push an alpha prerelease into the beta manifest', () => {
        expect(() => buildChannelManifest({
            channel: 'limo',
            version: '0.7.0-alpha.1',
            signature,
            url: 'https://releases.example.com/Folia_0.7.0_x64-setup.exe',
        })).toThrow(/not allowed on channel limo/);
    });

    it('refuses update-disabled channels and non-https urls', () => {
        expect(() => buildChannelManifest({
            channel: 'internal',
            version: '0.7.0',
            signature,
            url: 'https://releases.example.com/Folia_0.7.0_x64-setup.exe',
        })).toThrow(/update-disabled channel/);

        expect(() => buildChannelManifest({
            channel: 'realeco',
            version: '0.7.0',
            signature,
            url: 'http://releases.example.com/Folia_0.7.0_x64-setup.exe',
        })).toThrow(/must be https/);
    });

    it('rejects malformed versions and empty signatures', () => {
        expect(() => buildChannelManifest({
            channel: 'realeco',
            version: 'banana',
            signature,
            url: 'https://releases.example.com/x.exe',
        })).toThrow(/Invalid version/);

        expect(() => buildChannelManifest({
            channel: 'realeco',
            version: '0.7.0',
            signature: '   ',
            url: 'https://releases.example.com/x.exe',
        })).toThrow(/non-empty signature/);
    });
});

describe('release channel registry', () => {
    it('exposes the same lane table as the Electron reference', () => {
        expect(Object.keys(RELEASE_CHANNELS).sort()).toEqual(['cielo', 'internal', 'limo', 'realeco']);
        expect(RELEASE_CHANNELS.realeco).toMatchObject({ updaterChannel: 'latest', allowPrerelease: false, updateEnabled: true });
        expect(RELEASE_CHANNELS.limo).toMatchObject({ updaterChannel: 'beta', allowPrerelease: true, rollingReleaseTag: 'limo' });
        expect(RELEASE_CHANNELS.cielo).toMatchObject({ updaterChannel: 'alpha', allowPrerelease: true, rollingReleaseTag: 'cielo' });
        expect(RELEASE_CHANNELS.internal).toMatchObject({ updaterChannel: null, updateEnabled: false });
    });
});
