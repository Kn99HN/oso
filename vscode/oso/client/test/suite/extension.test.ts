import * as assert from 'assert';

import {
  Diagnostic,
  DiagnosticSeverity,
  languages,
  Position,
  Uri,
  workspace,
} from 'vscode';

// Helper that waits for `n` diagnostics to appear and then returns them.
async function getDiagnostics(n: number): Promise<[Uri, Diagnostic[]][]> {
  let diagnostics: [Uri, Diagnostic[]][] = [];
  for (;;) {
    diagnostics = languages.getDiagnostics();
    if (diagnostics.length === n) break;
    if (diagnostics.length > n) throw new Error('too many diagnostics');
    await new Promise(r => setTimeout(r, 0));
  }
  return diagnostics;
}

suite('Diagnostics', () => {
  test('We receive a diagnostic for each Polar file in the workspace', async () => {
    const files = (await workspace.findFiles('*.polar'))
      .map(f => f.toString())
      .sort();
    const diagnostics = (await getDiagnostics(files.length)).sort();

    let [uri, [diagnostic]] = diagnostics[0];
    assert.strictEqual(uri.toString(), files[0]);
    assert.strictEqual(diagnostic.severity, DiagnosticSeverity.Warning);
    assert(diagnostic.range.start.isEqual(new Position(0, 0)));
    assert(diagnostic.range.end.isEqual(new Position(0, 0)));
    assert(
      diagnostic.message.startsWith(
        'Your policy does not contain an allow rule'
      )
    );

    [uri, [diagnostic]] = diagnostics[1];
    assert.strictEqual(uri.toString(), files[1]);
    assert.strictEqual(diagnostic.severity, DiagnosticSeverity.Error);
    assert(diagnostic.range.start.isEqual(new Position(0, 6)));
    assert(diagnostic.range.end.isEqual(new Position(0, 6)));
    assert.strictEqual(
      diagnostic.message,
      'hit the end of the file unexpectedly. Did you forget a semi-colon'
    );
  });
});
