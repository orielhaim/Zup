/**
 * Turning a byte stream into protocol lines.
 *
 * `--format jsonl` writes one JSON document per line, and the process writes them
 * through a pipe, so a "line" is not a thing the pipe delivers. It arrives in
 * chunks that split wherever the operating system felt like splitting it: in the
 * middle of a UTF-8 sequence, in the middle of a number, in the middle of nothing
 * in particular.
 *
 * Two things follow, and both are why this is a module rather than a `split('\n')`
 * on the finished output:
 *
 * - **UTF-8 has to be decoded across chunk boundaries.** A `chunk.toString('utf8')`
 *   per chunk turns a multi-byte character at a boundary into U+FFFD, and a JSON
 *   document that names a file with an emoji in it stops parsing. `TextDecoder`
 *   with `stream: true` keeps the partial sequence for the next chunk.
 * - **A line has to be delivered before the process exits, or the point is lost.**
 *   A build that reports its first failure in the first second is a build somebody
 *   can stop.
 *
 * The last line of a stream may arrive without a trailing newline, so `end` flushes
 * the decoder and the partial line. Dropping it would drop the result.
 */

/** Feed a class of stream's bytes to a framer. */
export interface Framed {
  /** Bytes. Anything a `Buffer` is. */
  push(chunk: Uint8Array): void
  /** The stream is over: deliver whatever is left. */
  end(): void
}

/** A framer that yields whole lines, in order, as soon as each is complete. */
export class LineFramer implements Framed {
  #decoder = new TextDecoder('utf-8')
  #buffer = ''
  #closed = false

  constructor(private readonly emit: (line: string) => void) {}

  push(chunk: Uint8Array): void {
    if (this.#closed) {
      throw new Error('a closed framer does not accept bytes')
    }
    // `stream: true` is the whole point: a multi-byte character split across two
    // chunks is one character, not two replacement characters.
    this.#buffer += this.#decoder.decode(chunk, { stream: true })
    let newline = this.#buffer.indexOf('\n')
    while (newline !== -1) {
      // `\r` because a Windows child writes CRLF. A bare trailing carriage return
      // would still parse as JSON, so leaving it in would be harmless - and a line
      // reader that cannot say why it strips is one nobody trusts.
      const line = this.#buffer.slice(0, newline).replace(/\r$/u, '')
      this.#buffer = this.#buffer.slice(newline + 1)
      if (line.trim().length > 0) {
        this.emit(line)
      }
      newline = this.#buffer.indexOf('\n')
    }
  }

  end(): void {
    if (this.#closed) {
      return
    }
    this.#closed = true
    // Flush the decoder first: a sequence the process never completed is still
    // bytes on the wire, and a line reader that discards them is a line reader
    // that discards a document.
    this.#buffer += this.#decoder.decode()
    const tail = this.#buffer
    this.#buffer = ''
    if (tail.trim().length > 0) {
      this.emit(tail.replace(/\r$/u, ''))
    }
  }
}

/**
 * A framer that accumulates the whole stream and hands it over at the end.
 *
 * The `--format json` case, where the contract is *one* document. Splitting it into
 * lines would invent a requirement zup does not have, so this one does not frame
 * anything - but it does decode across chunk boundaries, because the failure it
 * prevents is identical.
 */
export class WholeStream implements Framed {
  #decoder = new TextDecoder('utf-8')
  #text = ''

  constructor(private readonly emit: (text: string) => void) {}

  push(chunk: Uint8Array): void {
    this.#text += this.#decoder.decode(chunk, { stream: true })
  }

  end(): void {
    this.#text += this.#decoder.decode()
    if (this.#text.length > 0) {
      this.emit(this.#text)
    }
  }

  /** Everything read so far. Empty until the stream ends. */
  get text(): string {
    return this.#text
  }
}
