import { readFile } from 'fs/promises';
import path from 'path';
import { fileURLToPath } from 'url';
import { describe, expect, it, vi } from 'vitest';

// test/unit/network/electronShimVideoExport.test.ts
// M9 视频导出 shim 行为：getDisplayMedia 提前启动、哨兵 desktop 专属拦截、
// 普通 getUserMedia 放行、取消/错误清理、缺失用户激活如实报错、
// 原始 IPC 写文件（ArrayBuffer body + token 头）、拒绝不吞错。

const currentDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(currentDir, '../../..');
const shimSource = await readFile(path.join(repoRoot, 'public/electron-shim.js'), 'utf8');

// 与 Rust src-tauri/src/video_export.rs 的共享契约常量，必须保持一致。
const SENTINEL_SOURCE_ID = '__folia_tauri_getdisplaymedia_sentinel__';
const TOKEN_HEADER = 'X-Folia-Video-Export-Token';

type FakeWindow = {
    __TAURI__?: {
        core: { invoke: (...args: unknown[]) => Promise<unknown>; event: { listen: () => Promise<() => void> } };
    };
    location: { href: string };
    fetch: (...args: unknown[]) => unknown;
    electron?: Record<string, (...args: unknown[]) => Promise<unknown>>;
    [key: string]: unknown;
};

type FakeTrack = {
    kind: string;
    stop: () => void;
    applyConstraints?: ReturnType<typeof vi.fn>;
};

type FakeDisplayStream = {
    stoppedTracks: string[];
    videoTrack: FakeTrack;
    getTracks: () => FakeTrack[];
    getVideoTracks: () => FakeTrack[];
};

const makeDisplayStream = (): FakeDisplayStream => {
    const stoppedTracks: string[] = [];
    const videoTrack: FakeTrack = {
        kind: 'video',
        stop: () => stoppedTracks.push('video'),
        applyConstraints: vi.fn().mockResolvedValue(undefined),
    };
    const audioTrack: FakeTrack = { kind: 'audio', stop: () => stoppedTracks.push('audio') };
    return {
        stoppedTracks,
        videoTrack,
        getTracks: () => [videoTrack, audioTrack],
        getVideoTracks: () => [videoTrack],
    };
};

const makeMediaDevices = (getDisplayMedia = vi.fn(), getUserMedia = vi.fn()) => ({
    mediaDevices: { getDisplayMedia, getUserMedia },
});

const evaluateShim = (window: FakeWindow, navigator: Record<string, unknown>) => {
    const fn = new Function('window', 'navigator', 'URL', 'Headers', 'TextEncoder', shimSource);
    fn(window, navigator, URL, Headers, TextEncoder);
    return window;
};

const makeTauriWindow = (invokeImpl: (...args: unknown[]) => Promise<unknown>) => {
    const nativeFetch = vi.fn().mockResolvedValue({ ok: true, status: 200, text: async () => '' });
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

const sentinelConstraints = (overrides: Record<string, unknown> = {}) => ({
    audio: false,
    video: {
        mandatory: {
            chromeMediaSource: 'desktop',
            chromeMediaSourceId: SENTINEL_SOURCE_ID,
            maxWidth: 1280,
            maxHeight: 720,
            maxFrameRate: 60,
            ...overrides,
        },
    },
});

describe('electron-shim M9 video export', () => {
    it('starts getDisplayMedia synchronously before the Rust save-dialog invoke', async () => {
        const displayStream = makeDisplayStream();
        const invoke = vi.fn().mockImplementation((cmd: string) => {
            if (cmd === 'video_export_choose_path') {
                return Promise.resolve({ canceled: false, filePath: 'C:\\Videos\\song.mp4', writeToken: 've-1' });
            }
            return Promise.resolve(null);
        });
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(displayStream));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        const result = await window.electron!.chooseVideoExportPath!('Song-1920x1080.mp4', 'mp4', 'MP4 Video');

        // 采集先于对话框 invoke 启动（瞬时用户激活窗口内）。
        expect(devices.mediaDevices.getDisplayMedia.mock.invocationCallOrder[0])
            .toBeLessThan(invoke.mock.invocationCallOrder[0]);
        expect(invoke).toHaveBeenCalledWith('video_export_choose_path', {
            defaultName: 'Song-1920x1080.mp4',
            extension: 'mp4',
            displayName: 'MP4 Video',
        });
        expect(result).toEqual({ canceled: false, filePath: 'C:\\Videos\\song.mp4' });

        // 窗口捕获提示 + 无声 + 从默认文件名解析出的理想尺寸。
        const constraints = devices.mediaDevices.getDisplayMedia.mock.calls[0][0];
        expect(constraints.video.displaySurface).toBe('window');
        expect(constraints.audio).toBe(false);
        expect(constraints.video.width.ideal).toBe(1920);
        expect(constraints.video.height.ideal).toBe(1080);
    });

    it('waits for the display picker to resolve before invoking the save dialog', async () => {
        const displayStream = makeDisplayStream();
        const invoke = vi.fn().mockResolvedValue({ canceled: false, filePath: 'C:\\Videos\\song.mp4', writeToken: 've-1' });
        let resolvePicker!: (stream: FakeDisplayStream) => void;
        const pickerPromise = new Promise<FakeDisplayStream>((resolve) => { resolvePicker = resolve; });
        const devices = makeMediaDevices(vi.fn().mockReturnValue(pickerPromise));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        const pathPromise = window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video');
        // 选择器仍挂起：保存对话框 invoke 绝不提前发生（两对话框不重叠）。
        expect(invoke).not.toHaveBeenCalled();

        resolvePicker(displayStream);
        const result = await pathPromise;
        expect(invoke).toHaveBeenCalledWith('video_export_choose_path', {
            defaultName: 'Song.mp4',
            extension: 'mp4',
            displayName: 'MP4 Video',
        });
        expect(result).toEqual({ canceled: false, filePath: 'C:\\Videos\\song.mp4' });
    });

    it('never opens the save dialog when the display picker rejects', async () => {
        const pickerError = new Error('NotAllowedError: Permission denied');
        const invoke = vi.fn();
        let rejectPicker!: (error: Error) => void;
        const pickerPromise = new Promise<FakeDisplayStream>((_resolve, reject) => { rejectPicker = reject; });
        const devices = makeMediaDevices(vi.fn().mockReturnValue(pickerPromise));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        const pathPromise = window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video');
        expect(invoke).not.toHaveBeenCalled();

        rejectPicker(pickerError);
        // 原错误透传，且保存对话框从未被调用。
        await expect(pathPromise).rejects.toThrow('NotAllowedError: Permission denied');
        expect(invoke).not.toHaveBeenCalled();
    });

    it('late resolution of an obsolete attempt stops its tracks and never replaces the current attempt', async () => {
        const streamA = makeDisplayStream();
        const streamB = makeDisplayStream();
        const invoke = vi.fn().mockImplementation((cmd: string) => {
            if (cmd === 'video_export_choose_path') {
                return Promise.resolve({ canceled: false, filePath: 'C:\\Videos\\B.mp4', writeToken: 've-B' });
            }
            if (cmd === 'video_export_restore_window') {
                return Promise.resolve(true);
            }
            return Promise.resolve(null);
        });
        let resolvePickerA!: (stream: FakeDisplayStream) => void;
        let resolvePickerB!: (stream: FakeDisplayStream) => void;
        const pickerA = new Promise<FakeDisplayStream>((resolve) => { resolvePickerA = resolve; });
        const pickerB = new Promise<FakeDisplayStream>((resolve) => { resolvePickerB = resolve; });
        const getDisplayMedia = vi.fn()
            .mockReturnValueOnce(pickerA)
            .mockReturnValueOnce(pickerB);
        const getUserMedia = vi.fn();
        const devices = makeMediaDevices(getDisplayMedia, getUserMedia);
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        // 尝试 A 开始，选择器挂起。
        const attemptA = window.electron!.chooseVideoExportPath!('A.mp4', 'mp4', 'MP4 Video');
        // A 被中止（restore 清理路径取消当前尝试）。
        await window.electron!.restoreVideoExportWindow!();
        // 尝试 B 开始（并再次取消已过期的 A；B 的选择器仍挂起）。
        const attemptB = window.electron!.chooseVideoExportPath!('B.mp4', 'mp4', 'MP4 Video');
        const choosePathCalls = () => invoke.mock.calls.filter((call) => call[0] === 'video_export_choose_path');
        expect(choosePathCalls()).toHaveLength(0); // 保存对话框未提前打开

        // B 先解析：保存对话框只对 B 发生，B 持有自己的流。
        resolvePickerB(streamB);
        await expect(attemptB).resolves.toEqual({ canceled: false, filePath: 'C:\\Videos\\B.mp4' });
        expect(choosePathCalls()).toHaveLength(1);
        expect(choosePathCalls()[0]).toEqual([
            'video_export_choose_path',
            { defaultName: 'B.mp4', extension: 'mp4', displayName: 'MP4 Video' },
        ]);
        expect(streamB.stoppedTracks).toEqual([]);

        // A 晚归：立即停掉自己的轨道，且不能覆盖当前尝试的 B 流。
        resolvePickerA(streamA);
        await expect(attemptA).rejects.toThrow(/failed or was cancelled/i);
        expect(streamA.stoppedTracks).toEqual(['video', 'audio']);

        // 哨兵 getUserMedia 返回的是 B 的流，而不是 A。
        const sentinelStream = await devices.mediaDevices.getUserMedia(sentinelConstraints());
        expect(sentinelStream).toBe(streamB);
        expect(getUserMedia).not.toHaveBeenCalled();
    });

    it('returns the cached display stream only for the sentinel desktop getUserMedia call', async () => {
        const displayStream = makeDisplayStream();
        const invoke = vi.fn().mockResolvedValue({ canceled: false, filePath: 'C:\\Videos\\song.mp4', writeToken: 've-1' });
        const getUserMedia = vi.fn();
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(displayStream), getUserMedia);
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        await window.electron!.chooseVideoExportPath!('Song-1280x720.mp4', 'mp4', 'MP4 Video');

        const stream = await devices.mediaDevices.getUserMedia(sentinelConstraints());
        expect(stream).toBe(displayStream);
        // 原生 getUserMedia 未被调用：拦截层直接返回缓存流。
        expect(getUserMedia).not.toHaveBeenCalled();
        expect(displayStream.videoTrack.applyConstraints).toHaveBeenCalled();
    });

    it('passes ordinary getUserMedia calls through to the native implementation untouched', async () => {
        const nativeGetUserMedia = vi.fn().mockResolvedValue('camera-stream');
        const devices = makeMediaDevices(vi.fn(), nativeGetUserMedia);
        const window = makeTauriWindow(vi.fn());
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        const ordinary = { audio: true, video: { mandatory: { chromeMediaSource: 'camera' } } };
        await expect(devices.mediaDevices.getUserMedia(ordinary)).resolves.toBe('camera-stream');
        expect(nativeGetUserMedia).toHaveBeenCalledWith(ordinary);

        // 非 desktop 哨兵的桌面约束同样放行（普通桌面采集不受影响）。
        const otherDesktop = { video: { mandatory: { chromeMediaSource: 'desktop', chromeMediaSourceId: 'other-window-id' } } };
        await devices.mediaDevices.getUserMedia(otherDesktop);
        expect(nativeGetUserMedia).toHaveBeenCalledWith(otherDesktop);
        expect(nativeGetUserMedia).toHaveBeenCalledTimes(2);
    });

    it('stops the pending display stream when the save dialog is canceled', async () => {
        const displayStream = makeDisplayStream();
        const invoke = vi.fn().mockResolvedValue({ canceled: true, filePath: null });
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(displayStream));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        const result = await window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video');
        expect(result).toEqual({ canceled: true, filePath: null });
        await Promise.resolve(); // 等缓存 promise 的 then 记录 pendingStream
        await Promise.resolve();
        expect(displayStream.stoppedTracks).toEqual(['video', 'audio']);
    });

    it('stops the pending display stream when the save dialog invoke fails', async () => {
        const displayStream = makeDisplayStream();
        const invoke = vi.fn().mockRejectedValue(new Error('dialog crashed'));
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(displayStream));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        await expect(window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video')).rejects.toThrow('dialog crashed');
        await Promise.resolve();
        await Promise.resolve();
        expect(displayStream.stoppedTracks).toEqual(['video', 'audio']);
    });

    it('cleans up the pending display stream on restore when it was never consumed', async () => {
        const displayStream = makeDisplayStream();
        const invoke = vi.fn().mockImplementation((cmd: string) => {
            if (cmd === 'video_export_choose_path') {
                return Promise.resolve({ canceled: false, filePath: 'C:\\Videos\\song.mp4', writeToken: 've-1' });
            }
            return Promise.resolve(true);
        });
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(displayStream));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        await window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video');
        await window.electron!.restoreVideoExportWindow!();
        await Promise.resolve();
        await Promise.resolve();
        expect(displayStream.stoppedTracks).toEqual(['video', 'audio']);
    });

    it('propagates a getDisplayMedia rejection honestly when there is no user activation', async () => {
        const displayError = new Error('NotAllowedError: Permission denied');
        const invoke = vi.fn();
        const devices = makeMediaDevices(vi.fn().mockRejectedValue(displayError));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        // 选择器拒绝：原错误透传，保存对话框 invoke 绝不发生，不做假流。
        await expect(window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video'))
            .rejects.toThrow('NotAllowedError: Permission denied');
        expect(invoke).not.toHaveBeenCalled();
    });

    it('rejects when getDisplayMedia is unavailable in the webview', async () => {
        const invoke = vi.fn();
        const window = makeTauriWindow(invoke);
        const devices = { mediaDevices: { getUserMedia: vi.fn() } }; // 无 getDisplayMedia
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        await expect(window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video'))
            .rejects.toThrow(/not available/i);
        expect(invoke).not.toHaveBeenCalled();
    });

    it('invokes writeVideoExportFile with a raw ArrayBuffer body and the token in an ASCII header', async () => {
        const token = 've-1234abc';
        const invoke = vi.fn().mockImplementation((cmd: string) => {
            if (cmd === 'video_export_choose_path') {
                return Promise.resolve({ canceled: false, filePath: 'C:\\Videos\\song.mp4', writeToken: token });
            }
            if (cmd === 'video_export_write_file') {
                return Promise.resolve(true);
            }
            return Promise.resolve(null);
        });
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(makeDisplayStream()));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        await window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video');
        const data = new Uint8Array([1, 2, 3, 4]).buffer;
        await expect(window.electron!.writeVideoExportFile!('C:\\Videos\\song.mp4', data)).resolves.toBe(true);

        // 原始 IPC：payload 即二进制，token 走 ASCII 请求头。
        const writeCall = invoke.mock.calls.find((call) => call[0] === 'video_export_write_file');
        expect(writeCall).toBeDefined();
        expect(writeCall![1]).toBe(data);
        expect(writeCall![2]).toEqual({ headers: { [TOKEN_HEADER]: token } });
    });

    it('rejects writeVideoExportFile when no token was issued for the path', async () => {
        const invoke = vi.fn().mockResolvedValue({ canceled: false, filePath: 'C:\\Videos\\song.mp4', writeToken: 've-1' });
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(makeDisplayStream()));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        await window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video');
        await expect(window.electron!.writeVideoExportFile!('C:\\Videos\\other.mp4', new Uint8Array([1]).buffer))
            .rejects.toThrow(/token/i);
        expect(invoke.mock.calls.some((call) => call[0] === 'video_export_write_file')).toBe(false);
    });

    it('propagates write failures instead of faking success', async () => {
        const invoke = vi.fn().mockImplementation((cmd: string) => {
            if (cmd === 'video_export_choose_path') {
                return Promise.resolve({ canceled: false, filePath: 'C:\\Videos\\song.mp4', writeToken: 've-1' });
            }
            if (cmd === 'video_export_write_file') {
                return Promise.reject(new Error('disk full'));
            }
            return Promise.resolve(true);
        });
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(makeDisplayStream()));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);

        await window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video');
        await expect(window.electron!.writeVideoExportFile!('C:\\Videos\\song.mp4', new Uint8Array([1]).buffer))
            .rejects.toThrow('disk full');
    });

    it('propagates getMainWindowCaptureSource rejections and returns sentinel results verbatim', async () => {
        const rejectingInvoke = vi.fn().mockRejectedValue(new Error('command failed'));
        const window = makeTauriWindow(rejectingInvoke);
        evaluateShim(window, { mediaDevices: { getDisplayMedia: vi.fn(), getUserMedia: vi.fn() } } as unknown as Record<string, unknown>);
        await expect(window.electron!.getMainWindowCaptureSource!()).rejects.toThrow('command failed');

        const sentinel = { id: SENTINEL_SOURCE_ID, name: 'Folia' };
        const okInvoke = vi.fn().mockResolvedValue(sentinel);
        const window2 = makeTauriWindow(okInvoke);
        evaluateShim(window2, { mediaDevices: { getDisplayMedia: vi.fn(), getUserMedia: vi.fn() } } as unknown as Record<string, unknown>);
        await expect(window2.electron!.getMainWindowCaptureSource!()).resolves.toEqual(sentinel);
        await expect(window2.electron!.getMainWindowCaptureSource!()).resolves.toEqual(sentinel); // 重复调用也如实返回
    });

    it('propagates prepareVideoExportWindow rejections and normalizes boolean results', async () => {
        const okInvoke = vi.fn().mockResolvedValue(true);
        const window = makeTauriWindow(okInvoke);
        evaluateShim(window, { mediaDevices: { getDisplayMedia: vi.fn(), getUserMedia: vi.fn() } } as unknown as Record<string, unknown>);
        await expect(window.electron!.prepareVideoExportWindow!({ width: 1280, height: 720 })).resolves.toBe(true);
        expect(okInvoke).toHaveBeenCalledWith('video_export_prepare_window', { size: { width: 1280, height: 720 } });

        const falseInvoke = vi.fn().mockResolvedValue(false);
        const window2 = makeTauriWindow(falseInvoke);
        evaluateShim(window2, { mediaDevices: { getDisplayMedia: vi.fn(), getUserMedia: vi.fn() } } as unknown as Record<string, unknown>);
        await expect(window2.electron!.prepareVideoExportWindow!({ width: 100, height: 100 })).resolves.toBe(false);

        const rejectingInvoke = vi.fn().mockRejectedValue(new Error('resize failed'));
        const window3 = makeTauriWindow(rejectingInvoke);
        evaluateShim(window3, { mediaDevices: { getDisplayMedia: vi.fn(), getUserMedia: vi.fn() } } as unknown as Record<string, unknown>);
        await expect(window3.electron!.prepareVideoExportWindow!({ width: 1280, height: 720 })).rejects.toThrow('resize failed');
    });

    it('resolves restoreVideoExportWindow to false on command failure (renderer fires-and-forgets)', async () => {
        const invoke = vi.fn().mockRejectedValue(new Error('restore failed'));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, { mediaDevices: { getDisplayMedia: vi.fn(), getUserMedia: vi.fn() } } as unknown as Record<string, unknown>);
        await expect(window.electron!.restoreVideoExportWindow!()).resolves.toBe(false);
        await expect(window.electron!.restoreVideoExportWindow!()).resolves.toBe(false);
    });

    it('never fakes success: chooseVideoExportPath rejection reaches the renderer', async () => {
        const invoke = vi.fn().mockRejectedValue(new Error('untrusted renderer'));
        const devices = makeMediaDevices(vi.fn().mockResolvedValue(makeDisplayStream()));
        const window = makeTauriWindow(invoke);
        evaluateShim(window, devices as unknown as Record<string, unknown>);
        await expect(window.electron!.chooseVideoExportPath!('Song.mp4', 'mp4', 'MP4 Video')).rejects.toThrow('untrusted renderer');
    });
});
