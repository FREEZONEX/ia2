/** Fit the entire authored screen inside the measured viewport, including its
 * padding. A short workbench pane must not hide the screen's bottom controls. */
export function fitCanvasScale(width: number, height: number, screenWidth: number, screenHeight: number): number {
  if (![width, height, screenWidth, screenHeight].every(Number.isFinite)
    || screenWidth <= 0 || screenHeight <= 0 || width <= 0 || height <= 0) return 1
  return Math.min(1, width / screenWidth, height / screenHeight)
}
