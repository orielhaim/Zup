import { describe, expect, it } from 'bun:test'

import { tokenize } from '../src/inputs.js'

/**
 * Argument tokenization is the action's only shell-shaped surface, and these
 * cases are the ones that actually occur: a project in `C:\Program Files`, a
 * signing command with a quoted argument, an empty `args` input.
 */
describe('tokenize', () => {
  it('splits on whitespace and collapses runs', () => {
    expect(tokenize('  --force   --target  x64 ', 'linux')).toEqual(['--force', '--target', 'x64'])
  })

  it('returns nothing for empty and whitespace-only input', () => {
    expect(tokenize('', 'linux')).toEqual([])
    expect(tokenize('   \n\t ', 'linux')).toEqual([])
  })

  it('keeps a quoted value as one argument', () => {
    expect(tokenize('--notes "two words"', 'linux')).toEqual(['--notes', 'two words'])
    expect(tokenize("--notes 'two words'", 'linux')).toEqual(['--notes', 'two words'])
  })

  it('preserves an empty quoted argument, which is a real argument', () => {
    expect(tokenize('--notes ""', 'linux')).toEqual(['--notes', ''])
  })

  it('handles a windows path with spaces as one argument', () => {
    expect(tokenize('--output "C:\\Program Files\\Acme"', 'win32')).toEqual([
      '--output',
      'C:\\Program Files\\Acme',
    ])
  })

  it('does not treat a windows backslash as an escape', () => {
    // The failure this prevents: `C:\temp` silently becoming `C:temp`.
    expect(tokenize('C:\\temp\\dist', 'win32')).toEqual(['C:\\temp\\dist'])
    expect(tokenize('C:\\temp\\dist', 'linux')).toEqual(['C:tempdist'])
  })

  it('handles a UNC path on windows', () => {
    expect(tokenize('\\\\server\\share\\dist', 'win32')).toEqual(['\\\\server\\share\\dist'])
  })

  it('keeps a single quote literal on windows, where the shell does', () => {
    expect(tokenize("don't", 'win32')).toEqual(["don't"])
  })

  it('keeps shell metacharacters as ordinary characters', () => {
    const hostile = 'a;rm -rf / | tee $(whoami) `id` && echo done'
    expect(tokenize(hostile, 'linux')).toEqual([
      'a;rm',
      '-rf',
      '/',
      '|',
      'tee',
      '$(whoami)',
      '`id`',
      '&&',
      'echo',
      'done',
    ])
  })

  it('honours a posix backslash escape outside quotes', () => {
    expect(tokenize('a\\ b', 'linux')).toEqual(['a b'])
    expect(tokenize('--output /tmp/a\\ b', 'linux')).toEqual(['--output', '/tmp/a b'])
  })

  it('treats a single quote inside double quotes as literal', () => {
    expect(tokenize('--notes "it\'s here"', 'linux')).toEqual(['--notes', "it's here"])
  })

  it('treats a double quote inside single quotes as literal', () => {
    expect(tokenize('--notes \'say "hi"\'', 'linux')).toEqual(['--notes', 'say "hi"'])
  })

  it('refuses an unclosed single quote, which posix would silently repair', () => {
    // Bash drops an unmatched quote and continues. That is the wrong behaviour
    // here: a mangled argument is a build that fails somewhere unrelated, and
    // refusing at the input is the one place the developer can see the mistake.
    expect(() => tokenize("it's", 'linux')).toThrow(/unclosed quote/u)
    expect(tokenize("it's", 'win32')).toEqual(["it's"])
  })

  it('rejects an unclosed quote rather than guessing', () => {
    expect(() => tokenize('--notes "unterminated', 'linux')).toThrow(/unclosed quote/u)
  })

  it('handles a unicode path', () => {
    expect(tokenize('--output "C:\\사용자\\프로젝트"', 'win32')).toEqual([
      '--output',
      'C:\\사용자\\프로젝트',
    ])
  })

  it('handles newlines, since a multi-line input is idiomatic YAML', () => {
    expect(tokenize('--force\n--prerelease', 'linux')).toEqual(['--force', '--prerelease'])
  })
})
