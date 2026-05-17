//! Gateway health and status — now handled by frontend via fetch() to Gateway HTTP API
//
// Health checks and system status are queried directly by the frontend
// using getGatewayUrl(), ensuring correct behavior in both local and
// remote Gateway scenarios.
