import { describe, expect, it } from 'vitest';
import { shouldUseCustomWindowRadius } from '@/components/app/presentation/windowChrome';

describe('shouldUseCustomWindowRadius', () => {
    it('keeps rounded transparent chrome for Electron', () => {
        expect(shouldUseCustomWindowRadius({
            hasElectronBridge: true,
            isTauriRuntime: false,
            transparentPlayerBackground: true,
        })).toBe(true);
    });

    it('does not mistake the Tauri compatibility bridge for Electron', () => {
        expect(shouldUseCustomWindowRadius({
            hasElectronBridge: true,
            isTauriRuntime: true,
            transparentPlayerBackground: true,
        })).toBe(false);
    });

    it('does not round an opaque window', () => {
        expect(shouldUseCustomWindowRadius({
            hasElectronBridge: true,
            isTauriRuntime: false,
            transparentPlayerBackground: false,
        })).toBe(false);
    });
});
