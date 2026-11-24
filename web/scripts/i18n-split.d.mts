export type Dict = { [key: string]: string | Dict };
export function keysInSource(source: string): Set<string>;
export function eagerFiles(entry: string, lazyLoaders: string): { eager: Set<string>; lazyRoots: Set<string> };
export function keysInFiles(files: Iterable<string>): Set<string>;
export function splitDictionary(dict: Dict, used: ReadonlySet<string>): { core: Dict; rest: Dict };
export function splitJa(srcDir: string): { core: Dict; rest: Dict };
