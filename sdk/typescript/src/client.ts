import { AnomaliesClient } from "./anomalies";
import { createConfigAsync, type OxyConfig } from "./config";
import { MetricTreeClient } from "./metricTree";
import { readParquet } from "./parquet";
import { PeerCohortClient } from "./peerCohort";
import type { ApiError, AppDataResponse, AppItem, GetDisplaysResponse, TableData } from "./types";

/**
 * Oxy API Client for interacting with Oxy data
 */
export class OxyClient {
  private config: OxyConfig;
  private _metricTree: MetricTreeClient | undefined;
  private _anomalies: AnomaliesClient | undefined;
  private _peerCohort: PeerCohortClient | undefined;

  constructor(config: OxyConfig) {
    this.config = config;
  }

  /**
   * Metric-tree analyses: tree introspection, sensitivity, predict, explain,
   * and opportunity sizing. Lazily constructed on first access.
   *
   * @example
   * ```typescript
   * const tree = await client.metricTree.getTree();
   * const sens = await client.metricTree.getSensitivity("orders.net_revenue");
   * ```
   */
  get metricTree(): MetricTreeClient {
    if (!this._metricTree) {
      this._metricTree = new MetricTreeClient(
        this.config,
        // Bind so the helper keeps `this` even when destructured by the inner client.
        this.request.bind(this)
      );
    }
    return this._metricTree;
  }

  /**
   * Anomaly inbox: list / scan / update-status / explain. Lazily constructed
   * on first access. Pairs with `metricTree.explain` — anomaly rows carry a
   * cached `ExplainResult` populated by `anomalies.explain(id)`.
   *
   * @example
   * ```typescript
   * await client.anomalies.scan({ as_of: "2025-12-15" });
   * const { anomalies } = await client.anomalies.list({ status: "new" });
   * ```
   */
  get anomalies(): AnomaliesClient {
    if (!this._anomalies) {
      this._anomalies = new AnomaliesClient(this.config, this.request.bind(this));
    }
    return this._anomalies;
  }

  /**
   * Peer-cohort benchmarking: resolve a subject's peer cohort and every
   * peer's comparison against it. Lazily constructed on first access.
   *
   * @example
   * ```typescript
   * const result = await client.peerCohort.resolve({
   *   entity: "restaurant_id",
   *   measure: "orders.net_revenue",
   *   time_dimension: "orders.order_date",
   *   period: ["2025-09-01", "2025-09-30"],
   * });
   * ```
   */
  get peerCohort(): PeerCohortClient {
    if (!this._peerCohort) {
      this._peerCohort = new PeerCohortClient(this.config, this.request.bind(this));
    }
    return this._peerCohort;
  }

  /**
   * Creates an OxyClient instance asynchronously with support for postMessage authentication
   *
   * This is the recommended method when using the SDK in an iframe that needs to
   * obtain authentication from the parent window via postMessage.
   *
   * @param config - Optional configuration overrides
   * @returns Promise resolving to OxyClient instance
   * @throws Error if required configuration is missing
   * @throws PostMessageAuthTimeoutError if parent doesn't respond
   *
   * @example
   * ```typescript
   * // In an iframe - automatic postMessage auth
   * const client = await OxyClient.create({
   *   parentOrigin: 'https://app.example.com',
   *   projectId: 'my-project-id',
   *   baseUrl: 'https://api.oxy.tech'
   * });
   *
   * // Use the client normally
   * const apps = await client.listApps();
   * ```
   */
  static async create(config?: Partial<OxyConfig>): Promise<OxyClient> {
    const resolvedConfig = await createConfigAsync(config);
    return new OxyClient(resolvedConfig);
  }

  /**
   * Encodes a file path to base64 for use in API URLs.
   * Handles Unicode characters (e.g., emojis) properly in both Node.js and browser.
   */
  private encodePathBase64(path: string): string {
    if (typeof Buffer !== "undefined") {
      // Node.js environment
      return Buffer.from(path).toString("base64");
    } else {
      // Browser environment - handle Unicode properly
      return btoa(
        encodeURIComponent(path).replace(/%([0-9A-F]{2})/g, (_, p1) =>
          String.fromCharCode(parseInt(p1, 16))
        )
      );
    }
  }

  /**
   * Makes an authenticated HTTP request to the Oxy API
   */
  private async request<T>(endpoint: string, options: RequestInit = {}): Promise<T> {
    const url = `${this.config.baseUrl}${endpoint}`;

    const headers: Record<string, string> = {
      "Content-Type": "application/json",
      ...((options.headers as Record<string, string>) || {})
    };

    // Only add Authorization header if API key is provided (optional for local dev)
    if (this.config.apiKey) {
      headers.Authorization = `Bearer ${this.config.apiKey}`;
    }

    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), this.config.timeout || 30000);

    try {
      const response = await fetch(url, {
        ...options,
        headers,
        signal: controller.signal
      });

      clearTimeout(timeoutId);

      if (!response.ok) {
        const errorText = await response.text().catch(() => "Unknown error");
        const error: ApiError = {
          message: `API request failed: ${response.statusText}`,
          status: response.status,
          details: errorText
        };
        throw error;
      }

      // Handle binary responses
      const acceptHeader =
        typeof options.headers === "object" && options.headers !== null
          ? (options.headers as Record<string, string>).Accept
          : undefined;
      if (acceptHeader === "application/octet-stream") {
        return response.blob() as Promise<T>;
      }

      return response.json();
    } catch (error: unknown) {
      clearTimeout(timeoutId);

      if (error instanceof Error && error.name === "AbortError") {
        throw new Error(`Request timeout after ${this.config.timeout || 30000}ms`);
      }

      throw error;
    }
  }

  /**
   * Builds query parameters including optional branch
   */
  private buildQueryParams(additionalParams: Record<string, string> = {}): string {
    const params: Record<string, string> = { ...additionalParams };

    if (this.config.branch) {
      params.branch = this.config.branch;
    }

    const searchParams = new URLSearchParams(params);
    const queryString = searchParams.toString();
    return queryString ? `?${queryString}` : "";
  }

  /**
   * Lists all apps in the project
   *
   * @returns Array of app items
   *
   * @example
   * ```typescript
   * const apps = await client.listApps();
   * console.log('Available apps:', apps);
   * ```
   */
  async listApps(): Promise<AppItem[]> {
    const query = this.buildQueryParams();
    return this.request<AppItem[]>(`/${this.config.projectId}/apps${query}`);
  }

  /**
   * Gets data for a specific app
   *
   * @param appPath - Relative path to the app file (e.g., 'my-app.app.yml')
   * @returns App data response
   *
   * @example
   * ```typescript
   * const data = await client.getAppData('dashboard.app.yml');
   * if (data.error) {
   *   console.error('Error:', data.error);
   * } else {
   *   console.log('App data:', data.data);
   * }
   * ```
   */
  async getAppData(appPath: string): Promise<AppDataResponse> {
    const pathb64 = this.encodePathBase64(appPath);
    const query = this.buildQueryParams();
    return this.request<AppDataResponse>(`/${this.config.projectId}/apps/${pathb64}${query}`);
  }

  /**
   * Runs an app and returns fresh data (bypasses cache)
   *
   * @param appPath - Relative path to the app file
   * @returns App data response
   *
   * @example
   * ```typescript
   * const data = await client.runApp('dashboard.app.yml');
   * console.log('Fresh app data:', data.data);
   * ```
   */
  async runApp(appPath: string): Promise<AppDataResponse> {
    const pathb64 = this.encodePathBase64(appPath);
    const query = this.buildQueryParams();
    return this.request<AppDataResponse>(`/${this.config.projectId}/apps/${pathb64}/run${query}`, {
      method: "POST"
    });
  }

  /**
   * Gets display configurations for an app
   *
   * @param appPath - Relative path to the app file
   * @returns Display configurations with potential errors
   *
   * @example
   * ```typescript
   * const displays = await client.getDisplays('dashboard.app.yml');
   * displays.displays.forEach(d => {
   *   if (d.error) {
   *     console.error('Display error:', d.error);
   *   } else {
   *     console.log('Display:', d.display);
   *   }
   * });
   * ```
   */
  async getDisplays(appPath: string): Promise<GetDisplaysResponse> {
    const pathb64 = this.encodePathBase64(appPath);
    const query = this.buildQueryParams();
    return this.request<GetDisplaysResponse>(
      `/${this.config.projectId}/apps/${pathb64}/displays${query}`
    );
  }

  /**
   * Gets a file from the app state directory (e.g., generated charts, images)
   *
   * This is useful for retrieving generated assets like charts, images, or other
   * files produced by app workflows and stored in the state directory.
   *
   * @param filePath - Relative path to the file in state directory
   * @returns Blob containing the file data
   *
   * @example
   * ```typescript
   * // Get a generated chart image
   * const blob = await client.getFile('charts/sales-chart.png');
   * const imageUrl = URL.createObjectURL(blob);
   *
   * // Use in an img tag
   * document.querySelector('img').src = imageUrl;
   * ```
   *
   * @example
   * ```typescript
   * // Download a file
   * const blob = await client.getFile('exports/data.csv');
   * const a = document.createElement('a');
   * a.href = URL.createObjectURL(blob);
   * a.download = 'data.csv';
   * a.click();
   * ```
   */
  async getFile(filePath: string): Promise<Blob> {
    const pathb64 = this.encodePathBase64(filePath);
    const query = this.buildQueryParams();
    return this.request<Blob>(`/${this.config.projectId}/apps/file/${pathb64}${query}`, {
      headers: {
        Accept: "application/octet-stream"
      }
    });
  }

  /**
   * Gets a file URL for direct browser access
   *
   * This returns a URL that can be used directly in img tags, fetch calls, etc.
   * The URL includes authentication via query parameters.
   *
   * @param filePath - Relative path to the file in state directory
   * @returns Full URL to the file
   *
   * @example
   * ```typescript
   * const imageUrl = client.getFileUrl('charts/sales-chart.png');
   *
   * // Use directly in img tag (in environments where query-based auth is supported)
   * document.querySelector('img').src = imageUrl;
   * ```
   */
  getFileUrl(filePath: string): string {
    const pathb64 = this.encodePathBase64(filePath);
    const query = this.buildQueryParams();
    return `${this.config.baseUrl}/${this.config.projectId}/apps/file/${pathb64}${query}`;
  }

  /**
   * Fetches a parquet file and parses it into table data
   *
   * @param filePath - Relative path to the parquet file
   * @param limit - Maximum number of rows to return (default: 100)
   * @returns TableData with columns and rows
   *
   * @example
   * ```typescript
   * const tableData = await client.getTableData('data/sales.parquet', 50);
   * console.log(tableData.columns);
   * console.log(tableData.rows);
   * console.log(`Total rows: ${tableData.total_rows}`);
   * ```
   */
  async getTableData(filePath: string, limit: number = 100): Promise<TableData> {
    const blob = await this.getFile(filePath);
    const result = await readParquet(blob, "data", limit);

    return {
      columns: result.columns,
      rows: result.rows,
      total_rows: result.rowCount
    };
  }
}
