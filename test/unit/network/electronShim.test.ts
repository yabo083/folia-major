import { readFile } from 'fs/promises';
import path from 'path';
import { fileURLToPath } from 'url';
import { describe, expect, it, vi } from 'vitest';

// test/unit/network/electronShim.test.ts
// M5 CORS 中继：验证 electron-shim.js 的 window.fetch patch 行为——
// Tauri 运行时仅拦截白名单 host 的跨源 fetch，走 lyric_proxy_fetch 命令；
// 白名单外 / 非 Tauri 环境原样放行（不破坏 vitest / 普通浏览器）。

const currentDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(currentDir, '../../..');
const shimSource = await readFile(path.join(repoRoot, 'public/electron-shim.js'), 'utf8');

type FakeWindow = {
    __TAURI__?: { core: { invoke: (...args: unknown[]) => Promise<unknown>; event: { listen: () => Promise<() => void> } } };
    location: { href: string };
    fetch: (...args: unknown[]) => unknown;
    electron?: Record<string, (...args: unknown[]) => Promise<unknown>>;
    [key: string]: unknown;
};

// 在受控作用域里执行 shim（shim 依赖 window/URL/Headers/TextEncoder 全局）。
const evaluateShim = (window: FakeWindow) => {
    const fn = new Function(
        'window',
        'URL',
        'Headers',
        'TextEncoder',
        shimSource,
    );
    fn(window, URL, Headers, TextEncoder);
    return window;
};

const makeNativeFetch = () => {
    const native = vi.fn().mockResolvedValue({ ok: true, status: 200, text: async () => 'native' });
    return native;
};

const makeTauriWindow = (invokeImpl: (...args: unknown[]) => Promise<unknown>, nativeFetch: ReturnType<typeof makeNativeFetch>) => {
    return {
        __TAURI__: {
            core: {
                invoke: invokeImpl,
                event: { listen: () => Promise.resolve(() => undefined) },
            },
            event: { listen: () => Promise.resolve(() => undefined) },
        },
        location: { href: 'https://app.folia.local/' },
        fetch: nativeFetch,
    } as FakeWindow;
};

describe('electron-shim window.fetch relay', () => {
    it('does not patch fetch outside Tauri runtime', () => {
        const native = makeNativeFetch();
        const window = { location: { href: 'https://app.folia.local/' }, fetch: native } as FakeWindow;
        evaluateShim(window);
        expect(window.fetch).toBe(native);
        expect(window.electron).toBeUndefined();
    });

    it('patches fetch in Tauri runtime and relays allowlisted hosts via lyric_proxy_fetch', async () => {
        const invoke = vi.fn().mockResolvedValue({
            ok: true,
            status: 200,
            statusText: 'OK',
            headers: { 'content-type': 'application/json' },
            bodyText: '{"error_code":0}',
        });
        const native = makeNativeFetch();
        const window = makeTauriWindow(invoke, native);
        evaluateShim(window);

        const url = 'http://complexsearch.kugou.com/v2/search/song?keyword=hello';
        const init = { method: 'GET', headers: { 'User-Agent': 'test', 'KG-Rec': '1' }, credentials: 'omit' as const };
        const response = await (window.fetch as typeof native)(url, init);

        expect(invoke).toHaveBeenCalledWith('lyric_proxy_fetch', {
            url,
            init: { method: 'GET', headers: { 'User-Agent': 'test', 'KG-Rec': '1' } },
        });
        expect(response.ok).toBe(true);
        expect(response.status).toBe(200);
        expect(response.statusText).toBe('OK');
        expect(response.headers.get('content-type')).toBe('application/json');
        await expect(response.json()).resolves.toEqual({ error_code: 0 });
        await expect(response.text()).resolves.toBe('{"error_code":0}');
        expect(native).not.toHaveBeenCalled();
    });

    it('exposes a fetch-compatible blob()/arrayBuffer() from the relayed body', async () => {
        // 二进制保真：bodyData（base64）用于 arrayBuffer()/blob()，bodyText 仍是文本。
        const rawBytes = new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46]);
        const invoke = vi.fn().mockResolvedValue({
            ok: true,
            status: 200,
            statusText: 'OK',
            headers: { 'content-type': 'image/jpeg' },
            bodyText: 'jpeg-cover',
            bodyData: Buffer.from(rawBytes).toString('base64'),
        });
        const native = makeNativeFetch();
        const window = makeTauriWindow(invoke, native);
        evaluateShim(window);

        const response = await (window.fetch as typeof native)('https://y.gtimg.cn/music/photo_new/cover.jpg', { mode: 'cors' });
        expect(response.headers.get('content-type')).toBe('image/jpeg');
        const blob = await response.blob();
        expect(new Uint8Array(await blob.arrayBuffer())).toEqual(rawBytes);
        const buffer = new Uint8Array(await response.arrayBuffer());
        expect(buffer).toEqual(rawBytes);
        await expect(response.text()).resolves.toBe('jpeg-cover');
        await expect(response.json()).rejects.toThrow();
    });

    it('treats the amll 404->204 conversion as ok with empty body', async () => {
        const invoke = vi.fn().mockResolvedValue({
            ok: true,
            status: 204,
            statusText: 'No Content',
            headers: {},
            bodyText: '',
        });
        const native = makeNativeFetch();
        const window = makeTauriWindow(invoke, native);
        evaluateShim(window);

        const response = await (window.fetch as typeof native)(
            'https://amll-ttml-db.stevexmh.net/ncm/123?format=ttml',
            { method: 'GET' },
        );
        expect(response.ok).toBe(true);
        expect(response.status).toBe(204);
        await expect(response.text()).resolves.toBe('');
        await expect(response.json()).rejects.toThrow();
    });

    it('passes non-allowlisted and relative URLs through to native fetch', async () => {
        const invoke = vi.fn();
        const native = makeNativeFetch();
        const window = makeTauriWindow(invoke, native);
        evaluateShim(window);

        await (window.fetch as typeof native)('https://example.com/data');
        await (window.fetch as typeof native)('/api/lyric-proxy?url=x');
        await (window.fetch as typeof native)('http://127.0.0.1:30000/ncm/api', { credentials: 'include' });

        expect(invoke).not.toHaveBeenCalled();
        expect(native).toHaveBeenCalledTimes(3);
    });

    it('relays kugou kgimg.com subdomain cover fetches', async () => {
        const invoke = vi.fn().mockResolvedValue({
            ok: true,
            status: 200,
            statusText: 'OK',
            headers: {},
            bodyText: 'img',
        });
        const native = makeNativeFetch();
        const window = makeTauriWindow(invoke, native);
        evaluateShim(window);

        await (window.fetch as typeof native)('http://imge.kugou.com/cover/1.jpg', { mode: 'cors' });
        await (window.fetch as typeof native)('http://singerpic.kugou.com/2.jpg', { mode: 'cors' });
        expect(invoke).toHaveBeenCalledTimes(2);
        expect(native).not.toHaveBeenCalled();
    });

    it('rejects when the underlying proxy command rejects (fallback masks the error)', async () => {
        // shim 的 call() 对命令 reject 走安全兜底（fetchLyricProxy fallback reject），
        // 但绝不应落到 native fetch。
        const invoke = vi.fn().mockRejectedValue(new Error('forbidden'));
        const native = makeNativeFetch();
        const window = makeTauriWindow(invoke, native);
        evaluateShim(window);

        await expect(
            (window.fetch as typeof native)('https://kugou.com/x'),
        ).rejects.toThrow();
        expect(native).not.toHaveBeenCalled();
    });

    it('surfaces structured 403 responses from the command as a non-ok Response', async () => {
        const invoke = vi.fn().mockResolvedValue({
            ok: false,
            status: 403,
            statusText: 'Forbidden',
            headers: {},
            bodyText: 'Forbidden lyric proxy host: evil.com',
        });
        const native = makeNativeFetch();
        const window = makeTauriWindow(invoke, native);
        evaluateShim(window);

        const response = await (window.fetch as typeof native)('https://kugou.com/redirect');
        expect(response.ok).toBe(false);
        expect(response.status).toBe(403);
        await expect(response.text()).resolves.toContain('Forbidden');
        expect(native).not.toHaveBeenCalled();
    });
});
