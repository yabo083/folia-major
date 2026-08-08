import React, { useState, useEffect } from 'react';
import { Copy, Minus, Pin, PinOff, Radio, Square, X } from 'lucide-react';

export default function WindowControls({
    revealed,
    isDaylight = false,
    isMainWindowClickThroughEnabled = false,
}: {
    revealed: boolean;
    isDaylight?: boolean;
    isMainWindowClickThroughEnabled?: boolean;
}) {
    const [isMaximized, setIsMaximized] = useState(false);
    const [isAlwaysOnTop, setIsAlwaysOnTop] = useState(false);
    const electron = (window as any).electron;

    useEffect(() => {
        if (!electron) return;
        const checkMaximize = async () => setIsMaximized(await electron.isWindowMaximized());
        checkMaximize();
        window.addEventListener('resize', checkMaximize);
        return () => window.removeEventListener('resize', checkMaximize);
    }, [electron]);

    useEffect(() => {
        if (!electron?.getMainWindowAlwaysOnTop) return;
        const syncAlwaysOnTop = async () => {
            try {
                setIsAlwaysOnTop(Boolean(await electron.getMainWindowAlwaysOnTop()));
            } catch (error) {
                console.error('Failed to read main window always-on-top state', error);
            }
        };
        void syncAlwaysOnTop();
        window.addEventListener('focus', syncAlwaysOnTop);
        return () => window.removeEventListener('focus', syncAlwaysOnTop);
    }, [electron]);

    if (!electron) return null;

    const remoteControlVisible = revealed && !isMainWindowClickThroughEnabled;
    const standardControlsVisible = revealed && !isMainWindowClickThroughEnabled;
    const remoteBtnClass = `flex items-center justify-center w-11 h-full transition-all duration-200 ${
        remoteControlVisible
            ? isDaylight
                ? 'pointer-events-auto opacity-85 translate-y-0 hover:opacity-100 hover:bg-black/[0.05]'
                : 'pointer-events-auto opacity-75 translate-y-0 hover:opacity-100 hover:bg-white/10'
            : 'pointer-events-none opacity-0 -translate-y-1'
    }`;
    const btnClass = `flex items-center justify-center w-11 h-full transition-all duration-200 ${
        standardControlsVisible
            ? isDaylight
                ? 'pointer-events-auto opacity-85 translate-y-0 hover:opacity-100 hover:bg-black/[0.05]'
                : 'pointer-events-auto opacity-75 translate-y-0 hover:opacity-100 hover:bg-white/10'
            : 'pointer-events-none opacity-0 -translate-y-1'
    }`;
    const closeBtnClass = `flex items-center justify-center w-11 h-full transition-all duration-200 ${
        standardControlsVisible
            ? isDaylight
                ? 'pointer-events-auto opacity-85 translate-y-0 hover:opacity-100 hover:bg-red-500 hover:text-white'
                : 'pointer-events-auto opacity-75 translate-y-0 hover:opacity-100 hover:bg-red-500 hover:text-white'
            : 'pointer-events-none opacity-0 -translate-y-1'
    }`;

    return (
        <div
            className={`flex h-full ${isDaylight ? 'text-zinc-800' : 'text-[var(--text-primary)]'}`}
            style={{
                WebkitAppRegion: 'no-drag',
                pointerEvents: 'none',
            } as React.CSSProperties}
        >
            <button
                className={remoteBtnClass}
                title="Remote control"
                tabIndex={remoteControlVisible ? 0 : -1}
                onClick={() => void electron.toggleRemoteControl?.().catch((error: unknown) => console.error('Failed to toggle remote control window', error))}
            >
                <Radio size={15} />
            </button>
            <button
                className={btnClass}
                title={isAlwaysOnTop ? 'Disable always on top' : 'Always on top'}
                tabIndex={standardControlsVisible ? 0 : -1}
                onClick={async () => {
                    try {
                        const current = Boolean(await electron.getMainWindowAlwaysOnTop());
                        setIsAlwaysOnTop(Boolean(await electron.setMainWindowAlwaysOnTop(!current)));
                    } catch (error) {
                        console.error('Failed to toggle main window always-on-top state', error);
                    }
                }}
            >
                {isAlwaysOnTop ? <PinOff size={15} /> : <Pin size={15} />}
            </button>
            <button
                className={btnClass}
                tabIndex={standardControlsVisible ? 0 : -1}
                onClick={() => electron.minimizeWindow()}
            >
                <Minus size={16} />
            </button>
            <button
                className={btnClass}
                tabIndex={standardControlsVisible ? 0 : -1}
                onClick={async () => {
                    await electron.toggleMaximizeWindow();
                    setIsMaximized(await electron.isWindowMaximized());
                }}
            >
                {isMaximized ? <Copy size={13} /> : <Square size={13} />}
            </button>
            <button
                className={closeBtnClass}
                tabIndex={standardControlsVisible ? 0 : -1}
                onClick={() => electron.closeWindow()}
            >
                <X size={16} />
            </button>
        </div>
    );
}
