import { useCallback, useEffect, useRef, useState } from 'react';
import type React from 'react';
import type { RefObject } from 'react';
import type { MotionValue } from 'framer-motion';
import type { SongResult, StatusMessage } from '../types';
import type { RemoteControlCommand } from '../types/remoteControl';
import type { VideoExportPreset, VideoExportState } from '../types/videoExport';
import { idleVideoExportState } from '../types/videoExport';
import {
    buildDefaultVideoExportFileName,
    getAudioElementCaptureStream,
    getMainWindowVideoCaptureStream,
    getVideoExportRecorderOptions,
    getSupportedVideoExportFormat,
    installVideoExportCursorGuard,
    stopMediaStream,
    wait,
} from '../services/electronVideoExport';
import {
    clearVideoExportPendingRequest,
    consumeVideoExportPendingRequest,
    createEmptyVideoExportPendingRequestState,
    storeVideoExportPendingRequest,
} from '../services/videoExportPendingRequest';

// src/hooks/useElectronVideoExportController.ts
// Records the real player window so audio.currentTime remains the single animation clock.
type UseElectronVideoExportControllerOptions = {
    t: (key: string) => string;
    isElectronWindow: boolean;
    audioRef: RefObject<HTMLAudioElement | null>;
    currentTime: MotionValue<number>;
    duration: number;
    currentSong: SongResult | null;
    setIsPlayerChromeHidden: React.Dispatch<React.SetStateAction<boolean>>;
    setIsPanelOpen: React.Dispatch<React.SetStateAction<boolean>>;
    setStatusMsg: React.Dispatch<React.SetStateAction<StatusMessage | null>>;
    navigateToPlayer: () => void;
    pausePlayback: () => void;
    resumePlayback: () => Promise<void>;
};

const COUNTDOWN_SECONDS = 3;

const toArrayBuffer = (blob: Blob) => blob.arrayBuffer();

export const useElectronVideoExportController = ({
    t,
    isElectronWindow,
    audioRef,
    currentTime,
    duration,
    currentSong,
    setIsPlayerChromeHidden,
    setIsPanelOpen,
    setStatusMsg,
    navigateToPlayer,
    pausePlayback,
    resumePlayback,
}: UseElectronVideoExportControllerOptions) => {
    const [exportState, setExportState] = useState<VideoExportState>(idleVideoExportState);
    const recorderRef = useRef<MediaRecorder | null>(null);
    const cancelRequestedRef = useRef(false);
    const runningRef = useRef(false);
    const pendingExportRef = useRef(createEmptyVideoExportPendingRequestState());

    const stopActiveExport = useCallback((discard: boolean) => {
        cancelRequestedRef.current = discard;
        const recorder = recorderRef.current;
        if (recorder && recorder.state !== 'inactive') {
            recorder.stop();
        }
    }, []);

    const startExport = useCallback(async (preset: VideoExportPreset, startMode: 'from-start' | 'current') => {
        if (!isElectronWindow || runningRef.current) {
            return;
        }

        const audioElement = audioRef.current;
        if (!audioElement || !currentSong) {
            setExportState({
                ...idleVideoExportState(),
                status: 'error',
                presetId: preset.id,
                error: t('export.noRecordableContent'),
            });
            return;
        }

        const electron = window.electron;
        if (!electron?.chooseVideoExportPath || !electron.getMainWindowCaptureSource || !electron.prepareVideoExportWindow || !electron.restoreVideoExportWindow || !electron.writeVideoExportFile) {
            setExportState({
                ...idleVideoExportState(),
                status: 'error',
                presetId: preset.id,
                error: t('export.windowRecordingUnsupported'),
            });
            return;
        }

        runningRef.current = true;
        cancelRequestedRef.current = false;
        let videoStream: MediaStream | null = null;
        let audioStream: MediaStream | null = null;
        let combinedStream: MediaStream | null = null;
        let progressIntervalId: number | null = null;
        let endedListener: (() => void) | null = null;
        let removeCursorGuard: (() => void) | null = null;
        const wasPaused = audioElement.paused;
        const previousLoop = audioElement.loop;
        const previousTime = audioElement.currentTime;

        try {
            const exportFormat = getSupportedVideoExportFormat();
            if (!exportFormat) {
                throw new Error(t('export.noExportCodec'));
            }

            const saveResult = await electron.chooseVideoExportPath(
                buildDefaultVideoExportFileName(currentSong, preset, exportFormat.extension),
                exportFormat.extension,
                exportFormat.displayName,
            );
            if (saveResult.canceled || !saveResult.filePath) {
                setExportState(idleVideoExportState());
                return;
            }

            const exportStartTime = startMode === 'from-start' ? 0 : Math.max(0, audioElement.currentTime);
            const safeDuration = Number.isFinite(duration) && duration > 0
                ? duration
                : audioElement.duration;
            const exportDuration = Number.isFinite(safeDuration) && safeDuration > exportStartTime
                ? safeDuration - exportStartTime
                : 0;

            setExportState({
                status: 'preparing',
                presetId: preset.id,
                progress: 0,
                elapsed: 0,
                duration: exportDuration,
                countdown: null,
                filePath: saveResult.filePath,
                error: null,
            });

            navigateToPlayer();
            setIsPanelOpen(false);
            setIsPlayerChromeHidden(true);
            removeCursorGuard = installVideoExportCursorGuard();
            pausePlayback();
            audioElement.pause();
            audioElement.loop = false;

            if (startMode === 'from-start') {
                audioElement.currentTime = 0;
                currentTime.set(0);
            }

            const prepared = await electron.prepareVideoExportWindow({ width: preset.width, height: preset.height });
            if (!prepared) {
                throw new Error(t('export.windowResizeFailed'));
            }
            await wait(300);
            videoStream = await getMainWindowVideoCaptureStream(preset);
            audioStream = getAudioElementCaptureStream(audioElement);
            combinedStream = new MediaStream([
                ...videoStream.getVideoTracks(),
                ...audioStream.getAudioTracks(),
            ]);

            for (let remaining = COUNTDOWN_SECONDS; remaining > 0; remaining -= 1) {
                setExportState(prev => ({
                    ...prev,
                    status: 'countdown',
                    countdown: remaining,
                }));
                await wait(1000);
                if (cancelRequestedRef.current) {
                    throw new Error(t('export.recordingCancelled'));
                }
            }

            const chunks: Blob[] = [];
            const recorder = new MediaRecorder(combinedStream, getVideoExportRecorderOptions(preset, exportFormat));
            recorderRef.current = recorder;
            const stopped = new Promise<void>((resolve, reject) => {
                recorder.ondataavailable = event => {
                    if (event.data.size > 0) {
                        chunks.push(event.data);
                    }
                };
                recorder.onerror = () => reject(new Error(t('export.recorderUnknownError')));
                recorder.onstop = () => resolve();
            });
            const requestStop = () => {
                if (recorder.state !== 'inactive') {
                    recorder.stop();
                }
            };
            endedListener = requestStop;
            audioElement.addEventListener('ended', requestStop, { once: true });

            recorder.start(1000);
            setExportState(prev => ({
                ...prev,
                status: 'recording',
                countdown: null,
            }));
            await resumePlayback();

            progressIntervalId = window.setInterval(() => {
                const elapsed = Math.max(0, audioElement.currentTime - exportStartTime);
                const progress = exportDuration > 0 ? Math.min(1, elapsed / exportDuration) : 0;
                setExportState(prev => ({
                    ...prev,
                    status: 'recording',
                    elapsed,
                    progress,
                }));

                if (exportDuration > 0 && elapsed >= exportDuration - 0.12) {
                    requestStop();
                }
            }, 250);

            await stopped;

            if (progressIntervalId !== null) {
                window.clearInterval(progressIntervalId);
                progressIntervalId = null;
            }

            if (cancelRequestedRef.current) {
                setExportState(idleVideoExportState());
                return;
            }

            setExportState(prev => ({
                ...prev,
                status: 'finalizing',
                progress: 1,
                elapsed: exportDuration,
            }));
            const blob = new Blob(chunks, { type: exportFormat.mimeType });
            await electron.writeVideoExportFile(saveResult.filePath, await toArrayBuffer(blob));
            setExportState(prev => ({
                ...prev,
                status: 'done',
                progress: 1,
                elapsed: exportDuration,
                filePath: saveResult.filePath,
            }));
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            setExportState({
                ...idleVideoExportState(),
                status: cancelRequestedRef.current ? 'idle' : 'error',
                presetId: preset.id,
                error: cancelRequestedRef.current ? null : message,
            });
        } finally {
            if (progressIntervalId !== null) {
                window.clearInterval(progressIntervalId);
            }
            if (endedListener) {
                audioElement.removeEventListener('ended', endedListener);
            }
            recorderRef.current = null;
            stopMediaStream(videoStream);
            stopMediaStream(audioStream);
            stopMediaStream(combinedStream);
            audioElement.loop = previousLoop;
            if (wasPaused) {
                audioElement.pause();
                audioElement.currentTime = previousTime;
                currentTime.set(previousTime);
            }
            setIsPlayerChromeHidden(false);
            removeCursorGuard?.();
            void electron.restoreVideoExportWindow();
            runningRef.current = false;
            cancelRequestedRef.current = false;
        }
    }, [audioRef, currentSong, currentTime, duration, isElectronWindow, navigateToPlayer, pausePlayback, resumePlayback, setIsPanelOpen, setIsPlayerChromeHidden]);

    const handleExportCommand = useCallback((command: RemoteControlCommand) => {
        if (command.type === 'start-export') {
            // Remote IPC events carry no transient user activation inside the main
            // document, so getDisplayMedia cannot start here. Store exactly one
            // pending request, bring the main window forward, and ask the user to
            // confirm with a real click on a persistent toast. Nothing is claimed
            // about recording/preparing while the user has not confirmed.
            if (runningRef.current) {
                return true; // an export is already active; ignore the request
            }
            pendingExportRef.current = storeVideoExportPendingRequest(
                pendingExportRef.current,
                command.preset,
                command.startMode,
            );
            const pending = pendingExportRef.current.pending!;
            // Best-effort: bring the main window forward. Promise.resolve guards
            // against `undefined.catch` when window.electron or focusMainWindow is
            // absent; a real rejected promise is still swallowed.
            void Promise.resolve(window.electron?.focusMainWindow?.()).catch(() => {});
            setStatusMsg({
                type: 'info',
                text: t('export.remoteConfirmPrompt'),
                actionLabel: t('export.remoteStartAction'),
                cancelLabel: t('export.remoteStartCancel'),
                persistent: true,
                nonce: pending.nonce,
                onAction: () => {
                    // Only a non-stale callback may act: consume exactly-once, then
                    // dismiss only a status whose nonce matches this request, and
                    // invoke startExport in this same click call stack (no await /
                    // microtask in between) so getDisplayMedia is initiated under
                    // the main document's transient activation.
                    const { state, request } = consumeVideoExportPendingRequest(
                        pendingExportRef.current,
                        pending.nonce,
                    );
                    if (request) {
                        pendingExportRef.current = state;
                        setStatusMsg(current => (current?.nonce === request.nonce ? null : current));
                        void startExport(request.preset, request.startMode);
                    }
                },
                onCancel: () => {
                    // A stale cancel (a newer start request replaced this prompt)
                    // must not dismiss the current prompt or an unrelated status.
                    const { state, request } = consumeVideoExportPendingRequest(
                        pendingExportRef.current,
                        pending.nonce,
                    );
                    if (request) {
                        pendingExportRef.current = state;
                        setStatusMsg(current => (current?.nonce === request.nonce ? null : current));
                    }
                },
            });
            return true;
        }

        if (command.type === 'stop-export' || command.type === 'cancel-export') {
            // Stop/cancel cancels both a pending (unconfirmed) request and any
            // active export. Capture the prompt nonce first, then dismiss only a
            // status that still shows the export prompt — an unrelated status may
            // have replaced it while the request stayed pending.
            const promptNonce = pendingExportRef.current.pending?.nonce;
            if (promptNonce !== undefined) {
                pendingExportRef.current = clearVideoExportPendingRequest(pendingExportRef.current);
                setStatusMsg(current => (current?.nonce === promptNonce ? null : current));
            }
            stopActiveExport(command.type === 'cancel-export');
            return true;
        }

        return false;
    }, [setStatusMsg, startExport, stopActiveExport, t]);

    // Automatically reset export status back to 'idle' after completion (3s) or error (4s)
    useEffect(() => {
        if (exportState.status === 'done') {
            const timer = window.setTimeout(() => {
                setExportState(prev => prev.status === 'done' ? idleVideoExportState() : prev);
            }, 3000);
            return () => window.clearTimeout(timer);
        }
        if (exportState.status === 'error') {
            const timer = window.setTimeout(() => {
                setExportState(prev => prev.status === 'error' ? idleVideoExportState() : prev);
            }, 4000);
            return () => window.clearTimeout(timer);
        }
    }, [exportState.status]);

    useEffect(() => () => {
        stopActiveExport(true);
        pendingExportRef.current = clearVideoExportPendingRequest(pendingExportRef.current);
    }, [stopActiveExport]);

    return {
        exportState,
        handleExportCommand,
    };
};
