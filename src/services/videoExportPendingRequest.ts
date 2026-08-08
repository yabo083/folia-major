// src/services/videoExportPendingRequest.ts
// Pure, deterministic "exactly one pending request" store used when a remote
// start-export command must be confirmed by a real click inside the main
// window. getDisplayMedia requires transient user activation, and that
// activation cannot cross IPC: the main document only gets it from an actual
// click on the confirmation toast. Until that click happens the request is
// merely *pending* — no recording/preparing state is claimed.
//
// Kept as plain functions over an explicit state object so the lifecycle is
// unit-testable without React/hook infrastructure.

import type { VideoExportPreset, VideoExportStartMode } from '../types/videoExport';

export type VideoExportPendingRequest = {
    preset: VideoExportPreset;
    startMode: VideoExportStartMode;
    /** Monotonic request id used to ignore stale toast actions after a replacement. */
    nonce: number;
};

export type VideoExportPendingRequestState = {
    pending: VideoExportPendingRequest | null;
    nextNonce: number;
};

export const createEmptyVideoExportPendingRequestState = (): VideoExportPendingRequestState => ({
    pending: null,
    nextNonce: 1,
});

/**
 * Stores a single pending start request. Repeated start requests replace the
 * previous one deterministically (the latest request wins) and bump the nonce.
 */
export const storeVideoExportPendingRequest = (
    state: VideoExportPendingRequestState,
    preset: VideoExportPreset,
    startMode: VideoExportStartMode,
): VideoExportPendingRequestState => ({
    pending: { preset, startMode, nonce: state.nextNonce },
    nextNonce: state.nextNonce + 1,
});

/**
 * Consumes the pending request exactly once for a given nonce. A stale action
 * (created for a request that was since replaced) is ignored: it neither
 * clears the newer pending request nor starts an export.
 */
export const consumeVideoExportPendingRequest = (
    state: VideoExportPendingRequestState,
    nonce: number,
): { state: VideoExportPendingRequestState; request: VideoExportPendingRequest | null } => {
    if (state.pending && state.pending.nonce === nonce) {
        return {
            state: { pending: null, nextNonce: state.nextNonce },
            request: state.pending,
        };
    }
    return { state, request: null };
};

/**
 * Clears any pending request without resetting the nonce counter, keeping
 * stale-action detection monotonic across stop/cancel/unmount.
 */
export const clearVideoExportPendingRequest = (
    state: VideoExportPendingRequestState,
): VideoExportPendingRequestState => ({
    pending: null,
    nextNonce: state.nextNonce,
});
