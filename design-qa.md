# Self-update dialog design QA

- Source visual truth: `/tmp/codex-clipboard-d227b4b0-7647-4d7c-aca6-9af7a75c1c31.png`
- Implementation screenshots:
  - `.codex-artifacts/ui-self-update/01-empty.jpg`
  - `.codex-artifacts/ui-self-update/02-invalid-file.jpg`
  - `.codex-artifacts/ui-self-update/03-dialog-invalid.jpg`
- Combined comparison: `.codex-artifacts/ui-self-update/04-before-after.jpg`
- Browser viewport: 1280 × 720 CSS px
- Source pixels: 1028 × 1313
- Implementation full-view pixels: 1280 × 720
- Implementation dialog crop: 440 × 556 CSS px / 440 × 556 pixels
- Density normalization: implementation captured at 1 CSS px per image pixel; source resized to 440 px wide for the combined comparison.
- State: invalid file selected and server validation rejected it.

## Full-view comparison evidence

The implementation uses the existing application typography, button styles, colors, border tokens and modal chrome. The optimized modal is content-height rather than filling most of the viewport. The failure state no longer displays a green 100% progress bar or an active “uploading” label.

## Focused-region comparison evidence

The dialog crop confirms that the selected file is represented separately with filename, size and a visible removal action. The error message appears directly after the validation explanation. The primary action is restored after failure, allowing retry without closing the dialog.

## Comparison history

1. Earlier P1: validation failure appeared together with “正在上传”, a green 100% bar and an error alert. Fixed by separating upload and validation phases and hiding/resetting progress on failure.
2. Earlier P2: the fixed-height modal left excessive empty space. Fixed by using content-driven dialog height and tighter vertical spacing.
3. Earlier P2: the chosen file was embedded in the drop zone with no direct removal action. Fixed with a filename/size card and “移除” button.
4. Post-fix evidence: `02-invalid-file.jpg` and `03-dialog-invalid.jpg`; the browser console contained no warnings or errors.

## Required fidelity surfaces

- Fonts and typography: existing application font stack and hierarchy retained; filename truncation and secondary size styling added.
- Spacing and layout rhythm: modal content is compact, aligned, and uses existing 6 px radii and spacing scale.
- Colors and visual tokens: existing accent, success, muted, border and error tokens reused; no new isolated palette introduced.
- Image and asset fidelity: no raster assets are required for this utility dialog; the former text-glyph upload arrow was removed.
- Copy and content: technical explanation was shortened to the four actual validation stages and the upgrade-version constraint.

## Interaction checks

- Opened More → 更新自己.
- Verified the primary action is disabled before selecting a file.
- Selected a local invalid test file through the file chooser.
- Verified filename, size and removal action.
- Submitted the file and received the server-side invalid-file error.
- Verified progress was cleared and retry controls were restored.
- Checked browser console: no errors or warnings.

## Findings

No actionable P0, P1 or P2 findings remain. A future P3 polish option is to add a compact success/reconnect countdown after a valid upgrade.

final result: passed
