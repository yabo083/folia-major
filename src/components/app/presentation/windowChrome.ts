interface CustomWindowRadiusOptions {
    hasElectronBridge: boolean;
    isTauriRuntime: boolean;
    transparentPlayerBackground: boolean;
}

export function shouldUseCustomWindowRadius({
    hasElectronBridge,
    isTauriRuntime,
    transparentPlayerBackground,
}: CustomWindowRadiusOptions): boolean {
    return hasElectronBridge && !isTauriRuntime && transparentPlayerBackground;
}
