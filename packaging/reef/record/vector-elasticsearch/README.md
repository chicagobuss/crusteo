# Reef into Elasticsearch, through Vector

Every prompt, reply, tool call and finished turn in the fleet, searchable in Kibana. Reef starts
[Vector](https://vector.dev) as an `exec` sink in `plain` mode and writes records to its stdin;
Vector bulk-writes them into a daily `reef-fleet-*` index. Recording is described in
[docs/reef/record.md](../../../../docs/reef/record.md).

This needs Docker. It runs Elasticsearch with security off on localhost only: for trying, not
for keeping anything.

1. Start Elasticsearch and Kibana:

   ```sh
   docker compose up -d
   ```

2. Tell Elasticsearch that `body` is text, before the first record makes an index. `body` is every
   record's searchable text in one field (a reply's text, a tool call's name, input and output, a
   finished turn's prompt and reply), so this is the field to search:

   ```sh
   curl -s -X PUT localhost:9200/_index_template/reef-fleet \
     -H 'Content-Type: application/json' -d @index-template.json
   ```

3. Copy `record.json` into Reef's data dir (`reef-data/` beside `hosts.json`), and change
   `/path/to/vector-elasticsearch` to where this folder is. Reef picks it up within a second; no
   restart. To try it with no fleet, the demo records too:

   ```sh
   reef --demo --data-dir ./demo-data    # with record.json in ./demo-data
   ```

   The sink starts at each agent's end. To index the history the daemons still hold as well, add
   `"from": "start"` to the sink before Reef first reads it (see
   [docs/reef/record.md](../../../../docs/reef/record.md)).

4. Wait for an agent to finish a turn (the demo's do every half minute), then count what landed
   and search it:

   ```sh
   curl -s 'localhost:9200/reef-fleet-*/_count'
   curl -s 'localhost:9200/reef-fleet-*/_search?q=body:build&size=3'
   ```

5. In Kibana, http://localhost:5601, add a data view for `reef-fleet-*` with `time` as its time
   field, and open Discover. Search `body`; `host`, `agent.title`, `workspace.labels` and `kind`
   are there for filters.

A reply that streamed, or a tool call that finished, is written again under the same id, so the
index holds the latest of each. An agent deleted on its host arrives as one more document, of kind
`forget` (Reef sends it whatever `kinds` says): this example only indexes it, so filter it out with
`NOT kind:forget`, or delete that agent's documents by `host` and `agent.id` with a job of your own. To stop: take the sink out of `record.json` (Reef
stops Vector), then `docker compose down -v`.
