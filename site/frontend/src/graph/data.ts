export type GraphKind = "raw" | "percentfromfirst" | "percentrelative";

// Parameters used to filter graph data
export interface GraphsSelector {
  start: string;
  end: string;
  kind: GraphKind;
  stat: string;
  benchmark: string | null;
  scenario: string | null;
  profile: string | null;
}

export interface Series {
  points: [number];
  interpolated_indices: Set<number>;
}

// Graph data received from the server
export interface GraphData {
  commits: [[number, string]];
  benchmarks: Dict<Dict<Dict<Series>>>;
}
