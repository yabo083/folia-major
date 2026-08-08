import { describe, expect, it } from 'vitest';
import {
    clearVideoExportPendingRequest,
    consumeVideoExportPendingRequest,
    createEmptyVideoExportPendingRequestState,
    storeVideoExportPendingRequest,
} from '@/services/videoExportPendingRequest';
import type { VideoExportPreset } from '@/types/videoExport';

// test/unit/services/videoExportPendingRequest.test.ts

const preset = (id: string): VideoExportPreset => ({
    id,
    label: `${id} label`,
    width: 1280,
    height: 720,
    orientation: 'landscape',
});

describe('videoExportPendingRequest', () => {
    it('starts empty and stores exactly one pending request', () => {
        const state = storeVideoExportPendingRequest(
            createEmptyVideoExportPendingRequestState(),
            preset('a'),
            'from-start',
        );
        expect(state.pending).toEqual({
            preset: preset('a'),
            startMode: 'from-start',
            nonce: 1,
        });
    });

    it('repeated start requests replace the pending one deterministically', () => {
        const first = storeVideoExportPendingRequest(
            createEmptyVideoExportPendingRequestState(),
            preset('a'),
            'from-start',
        );
        const second = storeVideoExportPendingRequest(first, preset('b'), 'current');
        // The latest request wins and the nonce advances.
        expect(second.pending).toEqual({
            preset: preset('b'),
            startMode: 'current',
            nonce: 2,
        });
    });

    it('consumes the request exactly once on action confirm', () => {
        const stored = storeVideoExportPendingRequest(
            createEmptyVideoExportPendingRequestState(),
            preset('a'),
            'current',
        );
        const { state, request } = consumeVideoExportPendingRequest(stored, stored.pending!.nonce);
        // The action handler receives the request (this is the only place an
        // export may actually start) and the pending slot is cleared.
        expect(request).toEqual(stored.pending);
        expect(state.pending).toBeNull();
        // A second consume cannot start anything.
        const again = consumeVideoExportPendingRequest(state, stored.pending!.nonce);
        expect(again.request).toBeNull();
    });

    it('ignores a stale action from a request that was replaced', () => {
        const first = storeVideoExportPendingRequest(
            createEmptyVideoExportPendingRequestState(),
            preset('a'),
            'from-start',
        );
        const second = storeVideoExportPendingRequest(first, preset('b'), 'current');
        // The old toast (nonce 1) fires after a newer request replaced it:
        // it must neither start an export nor clear the newer pending request.
        const { state, request } = consumeVideoExportPendingRequest(second, first.pending!.nonce);
        expect(request).toBeNull();
        expect(state.pending).toEqual(second.pending);
    });

    it('cancel clears the pending request', () => {
        const stored = storeVideoExportPendingRequest(
            createEmptyVideoExportPendingRequestState(),
            preset('a'),
            'from-start',
        );
        const cleared = clearVideoExportPendingRequest(stored);
        expect(cleared.pending).toBeNull();
        const { request } = consumeVideoExportPendingRequest(cleared, stored.pending!.nonce);
        expect(request).toBeNull();
    });

    it('keeps nonce monotonic across clear so pre-cancel actions stay stale', () => {
        const stored = storeVideoExportPendingRequest(
            createEmptyVideoExportPendingRequestState(),
            preset('a'),
            'from-start',
        );
        const cleared = clearVideoExportPendingRequest(stored);
        const restarted = storeVideoExportPendingRequest(cleared, preset('b'), 'current');
        // The pre-cancel action (nonce 1) must not start the new request (nonce 2).
        const { state, request } = consumeVideoExportPendingRequest(restarted, stored.pending!.nonce);
        expect(request).toBeNull();
        expect(state.pending).toEqual(restarted.pending);
    });
});
