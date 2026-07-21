export class TopoDB {
  static open(path: string): Promise<TopoDB>
  formatVersion(): Promise<number>
  close(): void
  [Symbol.dispose](): void
}
