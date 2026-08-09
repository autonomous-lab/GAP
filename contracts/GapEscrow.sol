// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/**
 * @title GapEscrow
 * @notice On-chain escrow for GAP contracts (Geta Agent Protocol, part 05).
 *
 * The GAP protocol settles agent-to-agent commerce. In production, the
 * escrow is enforced by this contract instead of a trusted node: funds
 * are held by code, released by code, and nobody — including the node —
 * can move them outside the protocol's state machine.
 *
 * Design notes:
 *  - One contract instance serves many GAP contracts, keyed by
 *    keccak256 of the GAP contract id.
 *  - Payment currency is a stablecoin (USDC/EURC) held in this contract.
 *  - Authorization mirrors the GAP spec:
 *      park     — the client (holds funds)
 *      release  — the client, after accepting the delivery
 *      refund   — the client, before execution or after a ruling
 *      dispute  — the client, on rejected delivery
 *      rule     — the arbitrator, with a split summing to 1.0
 *  - The arbitrator is registered per GAP contract at park time.
 *
 * The reference implementation (src/payment.rs) is the off-chain twin
 * of this contract: same state machine, same authorization rules.
 */

interface IERC20 {
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

contract GapEscrow {
    /// States mirroring EscrowState in src/payment.rs.
    enum State { Empty, Parked, Released, Refunded, Disputed, Ruled }

    struct EscrowRecord {
        State state;
        address client;
        address provider;
        address arbitrator;
        uint256 amount;      // in stablecoin smallest unit (6 or 18 decimals)
        uint256 contractHash; // keccak256 of the GAP contract id + terms
    }

    IERC20 public immutable token;

    /// contractHash -> escrow record
    mapping(uint256 => EscrowRecord) public escrows;

    event Parked(uint256 indexed contractHash, address client, uint256 amount);
    event Released(uint256 indexed contractHash, address provider, uint256 amount);
    event Refunded(uint256 indexed contractHash, address client, uint256 amount);
    event Disputed(uint256 indexed contractHash, address client);
    event Ruled(uint256 indexed contractHash, uint256 clientShare, uint256 providerShare);

    error NotClient(uint256 contractHash);
    error NotArbitrator(uint256 contractHash);
    error InvalidState(uint256 contractHash, State expected);
    error SplitMustSumToOne();
    error ZeroAmount();

    constructor(address _token) {
        token = IERC20(_token);
    }

    /// Hash a GAP contract id for on-chain reference.
    function hashContract(string calldata contractId) external pure returns (uint256) {
        return uint256(keccak256(abi.encodePacked(contractId)));
    }

    /**
     * @notice Client parks funds for a GAP contract.
     * @param contractHash keccak256 of the GAP contract id
     * @param provider     provider address (receives on release)
     * @param arbitrator   registered arbitrator address (for disputes)
     * @param amount       amount in token units
     */
    function park(uint256 contractHash, address provider, address arbitrator, uint256 amount)
        external
    {
        if (amount == 0) revert ZeroAmount();
        EscrowRecord storage escrow = escrows[contractHash];
        if (escrow.state != State.Empty) revert InvalidState(contractHash, State.Empty);

        // Pull the funds from the client's wallet.
        bool ok = token.transferFrom(msg.sender, address(this), amount);
        require(ok, "token transferFrom failed");

        escrow.state = State.Parked;
        escrow.client = msg.sender;
        escrow.provider = provider;
        escrow.arbitrator = arbitrator;
        escrow.amount = amount;

        emit Parked(contractHash, msg.sender, amount);
    }

    /**
     * @notice Client releases funds to the provider after accepting the
     * delivery (the off-chain acceptance carries the proof bundle hash).
     */
    function release(uint256 contractHash) external {
        EscrowRecord storage escrow = escrows[contractHash];
        if (escrow.state != State.Parked) revert InvalidState(contractHash, State.Parked);
        if (msg.sender != escrow.client) revert NotClient(contractHash);

        escrow.state = State.Released;
        uint256 amount = escrow.amount;
        escrow.amount = 0;

        bool ok = token.transfer(escrow.provider, amount);
        require(ok, "token transfer failed");

        emit Released(contractHash, escrow.provider, amount);
    }

    /**
     * @notice Client refunds the parked funds (contract cancelled before
     * execution, or a ruling against the provider).
     */
    function refund(uint256 contractHash) external {
        EscrowRecord storage escrow = escrows[contractHash];
        if (escrow.state != State.Parked) revert InvalidState(contractHash, State.Parked);
        if (msg.sender != escrow.client) revert NotClient(contractHash);

        escrow.state = State.Refunded;
        uint256 amount = escrow.amount;
        escrow.amount = 0;

        bool ok = token.transfer(escrow.client, amount);
        require(ok, "token transfer failed");

        emit Refunded(contractHash, escrow.client, amount);
    }

    /// Client disputes: funds stay locked until the arbitrator rules.
    function dispute(uint256 contractHash) external {
        EscrowRecord storage escrow = escrows[contractHash];
        if (escrow.state != State.Parked) revert InvalidState(contractHash, State.Parked);
        if (msg.sender != escrow.client) revert NotClient(contractHash);

        escrow.state = State.Disputed;
        emit Disputed(contractHash, msg.sender);
    }

    /**
     * @notice Arbitrator rules on a dispute, splitting the funds.
     * @param contractHash   the GAP contract
     * @param clientBasisPoints share to the client in basis points (0-10000)
     */
    function rule(uint256 contractHash, uint256 clientBasisPoints) external {
        EscrowRecord storage escrow = escrows[contractHash];
        if (escrow.state != State.Disputed) revert InvalidState(contractHash, State.Disputed);
        if (msg.sender != escrow.arbitrator) revert NotArbitrator(contractHash);
        if (clientBasisPoints > 10000) revert SplitMustSumToOne();

        escrow.state = State.Ruled;
        uint256 amount = escrow.amount;
        escrow.amount = 0;

        uint256 clientShare = (amount * clientBasisPoints) / 10000;
        uint256 providerShare = amount - clientShare;

        if (clientShare > 0) {
            bool ok = token.transfer(escrow.client, clientShare);
            require(ok, "token transfer failed");
        }
        if (providerShare > 0) {
            bool ok = token.transfer(escrow.provider, providerShare);
            require(ok, "token transfer failed");
        }

        emit Ruled(contractHash, clientShare, providerShare);
    }

    /// View helper: current state of a GAP contract's escrow.
    function stateOf(uint256 contractHash) external view returns (State) {
        return escrows[contractHash].state;
    }
}
