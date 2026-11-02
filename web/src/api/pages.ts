// The list endpoints are cursor pages (a page can be short, even empty,
// while `has_more` is true). The pages of this UI show whole lists, so they
// read pages until the end or until a cap, and say when the cap cut the
// list (`truncated`): never a silently shortened list.

export interface Page<T> {
  items: T[];
  has_more: boolean;
  next_cursor: string | null;
}

export interface All<T> {
  items: T[];
  /** The cap ended the reading while more rows existed. */
  truncated: boolean;
}

/** At most this many pages (100 rows each) per list. */
export const MAX_PAGES = 10;

export async function fetchAllPages<T>(
  fetchPage: (cursor: string | undefined) => Promise<Page<T>>,
  maxPages: number = MAX_PAGES,
): Promise<All<T>> {
  const items: T[] = [];
  let cursor: string | undefined;
  for (let n = 0; n < maxPages; n += 1) {
    const page = await fetchPage(cursor);
    items.push(...page.items);
    if (!page.has_more || page.next_cursor === null) {
      return { items, truncated: false };
    }
    cursor = page.next_cursor;
  }
  return { items, truncated: true };
}
