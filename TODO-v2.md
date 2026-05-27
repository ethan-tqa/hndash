# v2: HN Voting & Favoriting

## Goal

Add support for voting (upvote/downvote) and favoriting posts and comments directly from hndash.

## Problem

HN has **no official API** for write operations. The official Firebase API at `https://hacker-news.firebaseio.com/v0/` is read-only.

## Approach

The HN website itself uses undocumented endpoints that require cookie-based session auth. Implementation would require:

### 1. HN Login

- `POST https://news.ycombinator.com/login` with form fields `acct`, `pw`, `goto`
- Capture and persist the session cookie returned in the response

### 2. Scrape `auth` Token

- When logged in, vote/favorite links in HN page HTML include an `auth` query parameter
- Need to fetch a page (e.g. the item page) and extract the `auth` token from the vote link HTML
- Token appears to be per-session or per-page

### 3. Voting

- `POST https://news.ycombinator.com/vote` with params: `id` (item ID), `how` (`up` or `down`), `auth` (scraped token), `goto` (return path)
- Downvoting requires 501+ karma on HN

### 4. Favoriting

- `GET https://news.ycombinator.com/favorites?id=<id>&action=fave&auth=<token>`

## Risks

- Undocumented, could break at any time
- Requires storing HN credentials in the app config
- `auth` token scraping is fragile
- Rate limiting / ban risk if automated heavily

## Suggested Implementation Order

1. Add HN credentials (`username`, `password`) to `config.toml`
2. Add a login module that authenticates and stores session cookies
3. Add a token scraper for the `auth` parameter
4. Add vote/favorite functions
5. Add UI buttons to the dashboard template
6. Add API routes and DB tracking (optional: log votes/faves)
