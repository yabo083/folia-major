import React from 'react';
import { DEFAULT_VISUALIZER_BACKGROUND_MODE, getVisualizerBackgroundRegistryEntry } from './registry';
import type { VisualizerBackgroundRenderProps } from './definition';

// src/components/visualizer/backgrounds/VisualizerBackgroundRenderer.tsx
// Selects the active shell background through the discoverable background registry.

const VisualizerBackgroundRenderer: React.FC<VisualizerBackgroundRenderProps> = (props) => {
    if (props.config?.transparent) {
        // A fully empty WebView is unreadable over a light desktop. Keep the
        // window transparent while retaining a restrained, translucent player
        // surface so lyrics and controls remain legible.
        return (
            <div
                className="absolute inset-0 z-0"
                style={{
                    backgroundColor: `color-mix(in srgb, ${props.theme.backgroundColor} 84%, transparent)`,
                    backdropFilter: 'blur(18px)',
                    WebkitBackdropFilter: 'blur(18px)',
                }}
            />
        );
    }

    const mode = props.config?.mode ?? DEFAULT_VISUALIZER_BACKGROUND_MODE;
    return getVisualizerBackgroundRegistryEntry(mode).render(props);
};

export default VisualizerBackgroundRenderer;
