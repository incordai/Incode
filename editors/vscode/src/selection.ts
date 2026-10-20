/** The text `incode.sendSelection` puts in the chat input. Pure. */

export interface SelectionInfo {
  /** Workspace-relative path, as shown to the person and the model. */
  path: string;
  languageId: string;
  /** 1-based, inclusive. */
  startLine: number;
  endLine: number;
  text: string;
}

export function formatSelection(s: SelectionInfo): string {
  const where = s.startLine === s.endLine ? `${s.path}:${s.startLine}` : `${s.path}:${s.startLine}-${s.endLine}`;
  // A fence longer than any backtick run inside the text, so it cannot be closed early.
  const longest = Math.max(0, ...(s.text.match(/`+/g) ?? []).map((r) => r.length));
  const fence = "`".repeat(Math.max(3, longest + 1));
  const body = s.text.endsWith("\n") ? s.text : `${s.text}\n`;
  return `${where}\n${fence}${s.languageId}\n${body}${fence}\n`;
}
