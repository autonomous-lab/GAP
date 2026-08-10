// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/**
 * @title GapDeposit
 * @notice Funds a GAP agent's prefunded balance on a custodial node
 *         (RFC-0016 §5.3), and says whose balance it funds.
 *
 * @dev The problem this solves is attribution, not transport.
 *
 * A plain ERC-20 transfer to a node's wallet carries no indication of
 * which agent it belongs to. Deriving one address per agent works, but
 * then the operator must sweep N addresses, paying gas on each and
 * managing N keys that hold other people's money in the meantime.
 *
 * Here the agent identifier travels in the calldata and comes back out
 * in an indexed event, so one address serves every agent and there is
 * nothing to sweep.
 *
 * **This contract never holds anything.** Tokens move from the payer
 * straight to the treasury in the same call, so:
 *
 *  - there is no balance here for anyone to steal;
 *  - there is no `withdraw`, and therefore no owner, no admin key and
 *    no upgrade path to a rug pull;
 *  - a node that stops running does not strand funds inside it.
 *
 * The only thing it adds to a transfer is the sentence "this one is
 * for agent X", which is exactly the sentence the chain was missing.
 */
interface IERC20 {
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

contract GapDeposit {
    /// The stablecoin this node settles in.
    IERC20 public immutable token;

    /// Where deposited funds land. Immutable on purpose: a treasury an
    /// owner could repoint is an admin key wearing a different hat.
    address public immutable treasury;

    /**
     * @notice A deposit was made for a GAP agent.
     * @param agentId keccak256 of the agent's `did:gap:` identifier.
     *        Hashed rather than stored raw: a DID is long, and the node
     *        only ever needs to match it against one it already knows.
     * @param from    who paid. Recorded so a disputed credit can be
     *        traced to a payer, not just to an amount.
     * @param amount  in the token's own units.
     */
    event Deposited(bytes32 indexed agentId, address indexed from, uint256 amount);

    error ZeroAmount();
    error UnknownAgent();
    error TransferFailed();

    constructor(address _token, address _treasury) {
        token = IERC20(_token);
        treasury = _treasury;
    }

    /**
     * @notice Fund an agent's balance.
     * @dev The payer must have approved this contract for `amount`
     *      first, in the usual ERC-20 way.
     *
     *      Note there is deliberately no check that `agentId` exists:
     *      this contract has no idea what agents a node knows about,
     *      and pretending otherwise would put a registry on chain that
     *      immediately drifts from the node's own. An unrecognised id
     *      is caught off chain, where the answer actually lives.
     */
    function deposit(bytes32 agentId, uint256 amount) external {
        if (amount == 0) revert ZeroAmount();
        if (agentId == bytes32(0)) revert UnknownAgent();

        // Straight to the treasury: nothing accumulates here, so there
        // is nothing here to protect.
        bool ok = token.transferFrom(msg.sender, treasury, amount);
        if (!ok) revert TransferFailed();

        emit Deposited(agentId, msg.sender, amount);
    }

    /// @notice Helper for callers that hold the DID as a string.
    function agentIdOf(string calldata did) external pure returns (bytes32) {
        return keccak256(abi.encodePacked(did));
    }
}
