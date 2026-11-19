// The brand mark ("Balance", docs/brand/README.md): two identical bars moving in opposite
// directions — money moves, the sum stays equal. Drawn in currentColor on the 256 grid.
export const BRAND_MARK_PATH =
  "M86.5 64H232L184 112H86.5A24 24 0 0 1 86.5 64ZM72 144H169.5A24 24 0 0 1 169.5 192H24L72 144Z";

export function BrandMark({ size = 22 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 256 256" aria-hidden="true" focusable="false">
      <path fill="currentColor" d={BRAND_MARK_PATH} />
    </svg>
  );
}
