import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// test/unit/services/neteaseApiBase.test.ts
// Regression: the Netease frontend must route through the logged-in local proxy
// (`http://localhost:<port>` + `X-Folia-Cookie` header) and must never cache a
// broken base when the local API server has not reported a port yet. A rejected
// base is what lets the lyric-match modal surface a visible localized error
// instead of silently failing forever.

const mockJsonResponse = (payload: unknown) => ({
    json: vi.fn().mockResolvedValue(payload),
});

describe('netease local proxy base resolution', () => {
    let fetchMock: ReturnType<typeof vi.fn>;
    let storageMap: Record<string, string>;

    beforeEach(() => {
        vi.resetModules();
        vi.restoreAllMocks();
        vi.stubEnv('VITE_NETEASE_API_BASE', '');
        storageMap = {};
        vi.stubGlobal('localStorage', {
            getItem: vi.fn((key: string) => storageMap[key] || null),
            setItem: vi.fn((key: string, val: string) => { storageMap[key] = val; }),
            removeItem: vi.fn((key: string) => { delete storageMap[key]; }),
        });
        fetchMock = vi.fn();
        vi.stubGlobal('fetch', fetchMock);
    });

    afterEach(() => {
        vi.unstubAllEnvs();
        vi.unstubAllGlobals();
    });

    const stubElectronBridge = (port: number) => {
        vi.stubGlobal('window', {
            electron: {
                getNeteasePort: vi.fn().mockResolvedValue(port),
            },
        });
    };

    it('rejects and does not cache a base when the local server reports port 0, then recovers once a real port is available', async () => {
        stubElectronBridge(0);
        // Keep the anonymous-cookie pre-flight out of the way so the only
        // request we need to observe is the cloudsearch call itself.
        storageMap['netease_anonymous_cookie'] = 'anon-cookie';

        const { neteaseApi } = await import('@/services/netease');
        await expect(neteaseApi.cloudSearch('hello', 5, 0)).rejects.toThrow();

        // The broken base was never cached: once the bridge reports a real
        // port, the next call must reach the local proxy instead of failing.
        const bridge = vi.mocked((window as any).electron.getNeteasePort);
        bridge.mockResolvedValue(43123);
        fetchMock.mockResolvedValueOnce(mockJsonResponse({ result: { songs: [], songCount: 0 } }) as any);

        await expect(neteaseApi.cloudSearch('hello', 5, 0)).resolves.toMatchObject({ result: { songs: [] } });
        expect(String(fetchMock.mock.calls[0]?.[0])).toContain('http://localhost:43123/cloudsearch');
    });

    it('routes cloudsearch through the local proxy with the logged-in cookie header', async () => {
        storageMap['netease_cookie'] = 'real-login-cookie';
        stubElectronBridge(30000);
        fetchMock.mockResolvedValueOnce(mockJsonResponse({ result: { songs: [], songCount: 0 } }) as any);

        const { neteaseApi } = await import('@/services/netease');
        await neteaseApi.cloudSearch('Song Title', 50, 0);

        expect(fetchMock).toHaveBeenCalledTimes(1);
        const calledUrl = String(fetchMock.mock.calls[0]?.[0]);
        expect(calledUrl).toContain('http://localhost:30000/cloudsearch');
        expect(calledUrl).toContain('keywords=Song%20Title');
        // The cookie travels in the header, never in the URL query.
        expect(calledUrl).not.toContain('cookie=');
        expect(new Headers(fetchMock.mock.calls[0]?.[1]?.headers).get('X-Folia-Cookie')).toBe('real-login-cookie');
        expect(fetchMock.mock.calls[0]?.[1]?.credentials).toBe('include');
    });

    it('does not attempt an anonymous registration when a logged-in cookie is stored', async () => {
        storageMap['netease_cookie'] = 'real-login-cookie';
        stubElectronBridge(30000);
        fetchMock.mockResolvedValueOnce(mockJsonResponse({ result: { songs: [], songCount: 0 } }) as any);

        const { neteaseApi } = await import('@/services/netease');
        await neteaseApi.cloudSearch('x', 5, 0);

        expect(fetchMock).toHaveBeenCalledTimes(1);
        expect(String(fetchMock.mock.calls[0]?.[0])).not.toContain('register/anonimous');
    });
});
